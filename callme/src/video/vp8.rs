use std::{mem::MaybeUninit, ops::ControlFlow, ptr, slice};

use anyhow::{bail, Result};
use bytes::Bytes;
use tokio::sync::broadcast;
use tracing::info;
use vpx_sys::{
    vpx_codec_ctx_t, vpx_codec_dec_cfg_t, vpx_codec_err_t, vpx_codec_iter_t,
    VPX_CODEC_OK, VPX_DECODER_ABI_VERSION,
};

use crate::{
    codec::Codec,
    rtc::{MediaFrame, MediaTrack, TrackKind},
    video::VideoFrame,
};

/// VP8 encoder. Feed I420 [`VideoFrame`]s via [`push_frame`]; the encoded
/// bitstream is broadcast over a [`MediaTrack`] obtained at construction time.
pub struct Vp8Encoder {
    inner: vpx_encode::Encoder,
    pts: i64,
    rtp_duration: u32,
    sender: broadcast::Sender<MediaFrame>,
}

impl Vp8Encoder {
    /// Create a new encoder.
    ///
    /// - `fps`: target frame rate (used to compute RTP timestamp increments)
    /// - `bitrate_kbps`: target bitrate in kilobits per second
    /// - `track_cap`: capacity of the broadcast channel backing the [`MediaTrack`]
    pub fn new(
        width: u32,
        height: u32,
        fps: u32,
        bitrate_kbps: u32,
        track_cap: usize,
    ) -> Result<(Self, MediaTrack)> {
        let config = vpx_encode::Config {
            width,
            height,
            timebase: [1, crate::codec::VP8_RTP_CLOCK_RATE as i32],
            bitrate: bitrate_kbps,
            codec: vpx_encode::VideoCodecId::VP8,
        };
        let inner = vpx_encode::Encoder::new(config)
            .map_err(|e| anyhow::anyhow!("VP8 encoder init failed: {e}"))?;
        let (sender, receiver) = broadcast::channel(track_cap);
        let track = MediaTrack::new(receiver, Codec::Vp8, TrackKind::Video);
        let rtp_duration = crate::codec::VP8_RTP_CLOCK_RATE / fps;
        Ok((
            Self {
                inner,
                pts: 0,
                rtp_duration,
                sender,
            },
            track,
        ))
    }

    /// Encode one I420 frame and send the resulting [`MediaFrame`](s) to the track.
    pub fn push_frame(&mut self, frame: &VideoFrame) -> Result<ControlFlow<(), ()>> {
        for encoded in self
            .inner
            .encode(self.pts, &frame.data)
            .map_err(|e| anyhow::anyhow!("VP8 encode failed: {e}"))?
        {
            let media_frame = MediaFrame {
                payload: Bytes::copy_from_slice(encoded.data),
                sample_count: Some(self.rtp_duration),
                skipped_frames: None,
                skipped_samples: None,
            };
            if self.sender.send(media_frame).is_err() {
                info!("VP8 encoder: track receiver closed");
                return Ok(ControlFlow::Break(()));
            }
        }
        self.pts += self.rtp_duration as i64;
        Ok(ControlFlow::Continue(()))
    }
}

/// VP8 decoder. Feed encoded [`MediaFrame`] payloads via [`decode`]; get back
/// I420 [`VideoFrame`]s.
pub struct Vp8Decoder {
    ctx: Box<vpx_codec_ctx_t>,
}

impl Vp8Decoder {
    pub fn new() -> Result<Self> {
        // SAFETY: zeroed is a valid starting state; init_ver will populate all fields.
        let mut ctx: Box<vpx_codec_ctx_t> =
            Box::new(unsafe { MaybeUninit::zeroed().assume_init() });
        let cfg = vpx_codec_dec_cfg_t {
            threads: 1,
            w: 0,
            h: 0,
        };
        let err = unsafe {
            vpx_sys::vpx_codec_dec_init_ver(
                ctx.as_mut(),
                vpx_sys::vpx_codec_vp8_dx(),
                &cfg,
                0,
                VPX_DECODER_ABI_VERSION as i32,
            )
        };
        if err != VPX_CODEC_OK {
            bail!("VP8 decoder init failed: {err:?}");
        }
        Ok(Self { ctx })
    }

    /// Decode a VP8 bitstream payload. Returns the decoded I420 frame, or
    /// `None` if the decoder is still buffering (rare with VP8 realtime mode).
    pub fn decode(&mut self, data: &[u8]) -> Result<Option<VideoFrame>> {
        let err = unsafe {
            vpx_sys::vpx_codec_decode(
                self.ctx.as_mut(),
                data.as_ptr(),
                data.len() as u32,
                ptr::null_mut(),
                0,
            )
        };
        if err != VPX_CODEC_OK {
            bail!("VP8 decode failed: {err:?}");
        }

        let mut iter: vpx_codec_iter_t = ptr::null();
        let img_ptr =
            unsafe { vpx_sys::vpx_codec_get_frame(self.ctx.as_mut(), &mut iter) };
        if img_ptr.is_null() {
            return Ok(None);
        }

        let img = unsafe { &*img_ptr };
        let w = img.d_w as usize;
        let h = img.d_h as usize;
        let y_size = w * h;
        let uv_size = (w / 2) * (h / 2);
        let mut frame_data = vec![0u8; y_size + 2 * uv_size];

        // Copy Y, U, V planes respecting the decoder's internal strides.
        unsafe {
            for row in 0..h {
                let src = slice::from_raw_parts(
                    img.planes[0].add(row * img.stride[0] as usize),
                    w,
                );
                frame_data[row * w..(row + 1) * w].copy_from_slice(src);
            }
            for row in 0..h / 2 {
                let src = slice::from_raw_parts(
                    img.planes[1].add(row * img.stride[1] as usize),
                    w / 2,
                );
                let off = y_size + row * (w / 2);
                frame_data[off..off + w / 2].copy_from_slice(src);
            }
            for row in 0..h / 2 {
                let src = slice::from_raw_parts(
                    img.planes[2].add(row * img.stride[2] as usize),
                    w / 2,
                );
                let off = y_size + uv_size + row * (w / 2);
                frame_data[off..off + w / 2].copy_from_slice(src);
            }
        }

        Ok(Some(VideoFrame {
            width: w as u32,
            height: h as u32,
            data: frame_data,
        }))
    }
}

impl Drop for Vp8Decoder {
    fn drop(&mut self) {
        // SAFETY: ctx was initialised by dec_init_ver.
        unsafe {
            vpx_sys::vpx_codec_destroy(self.ctx.as_mut());
        }
    }
}

// SAFETY: libvpx encoder/decoder contexts are not thread-safe for concurrent
// access, but they are safe to move between threads (no self-referential ptrs).
unsafe impl Send for Vp8Encoder {}
unsafe impl Send for Vp8Decoder {}
