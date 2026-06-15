use std::ops::ControlFlow;

use anyhow::{bail, Result};
use tokio::sync::broadcast::error::TryRecvError;
use tracing::{info, trace, warn};

use crate::{
    codec::Codec,
    rtc::{MediaFrame, MediaTrack},
    video::{VideoFrame, VideoSource, vp8::Vp8Decoder},
};

/// Decodes a VP8-encoded [`MediaTrack`] into raw I420 [`VideoFrame`]s.
///
/// Analogous to [`crate::codec::opus::MediaTrackOpusDecoder`] for audio.
pub struct MediaTrackVp8Decoder {
    track: MediaTrack,
    decoder: Vp8Decoder,
}

impl MediaTrackVp8Decoder {
    pub fn new(track: MediaTrack) -> Result<Self> {
        match track.codec() {
            Codec::Vp8 => {}
            other => bail!("MediaTrackVp8Decoder requires VP8 codec, got {other:?}"),
        }
        Ok(Self { track, decoder: Vp8Decoder::new()? })
    }
}

impl VideoSource for MediaTrackVp8Decoder {
    /// Decode one pending frame from the track.
    ///
    /// - `Continue(Some(frame))` — a decoded frame is ready.
    /// - `Continue(None)` — no frame is buffered; call again after a short wait.
    /// - `Break(())` — the track is closed and no more frames will arrive.
    fn next_frame(&mut self) -> Result<ControlFlow<(), Option<VideoFrame>>> {
        let payload: Option<Vec<u8>> = match self.track.try_recv() {
            Ok(MediaFrame { payload, skipped_frames, .. }) => {
                if let Some(n) = skipped_frames.filter(|&n| n > 0) {
                    warn!("VP8 decoder: {n} frames skipped");
                }
                Some(payload.to_vec())
            }
            Err(TryRecvError::Empty) => return Ok(ControlFlow::Continue(None)),
            Err(TryRecvError::Lagged(n)) => {
                warn!("VP8 decoder lagged by {n} frames");
                return Ok(ControlFlow::Continue(None));
            }
            Err(TryRecvError::Closed) => {
                info!("VP8 decoder: track closed");
                return Ok(ControlFlow::Break(()));
            }
        };

        if let Some(data) = payload {
            trace!("VP8 decode {} bytes", data.len());
            match self.decoder.decode(&data)? {
                Some(frame) => Ok(ControlFlow::Continue(Some(frame))),
                None => Ok(ControlFlow::Continue(None)),
            }
        } else {
            Ok(ControlFlow::Continue(None))
        }
    }
}

// ── colour conversion ─────────────────────────────────────────────────────────

/// Convert a planar I420 [`VideoFrame`] to packed RGBA bytes.
///
/// Output length is `frame.width * frame.height * 4`.
/// Uses BT.601 limited-range coefficients, the inverse of [`super::capture::rgb_to_i420`].
pub fn i420_to_rgba(frame: &VideoFrame) -> Vec<u8> {
    let w = frame.width as usize;
    let h = frame.height as usize;
    let y_size = w * h;
    let uv_size = (w / 2) * (h / 2);

    let y_plane = &frame.data[..y_size];
    let u_plane = &frame.data[y_size..y_size + uv_size];
    let v_plane = &frame.data[y_size + uv_size..];

    let mut rgba = vec![0u8; w * h * 4];

    for row in 0..h {
        for col in 0..w {
            let y = y_plane[row * w + col] as i32;
            let u = u_plane[(row / 2) * (w / 2) + col / 2] as i32;
            let v = v_plane[(row / 2) * (w / 2) + col / 2] as i32;

            // BT.601 limited-range: Y ∈ [16,235], Cb/Cr ∈ [16,240]
            let c = y - 16;
            let d = u - 128;
            let e = v - 128;

            let r = ((298 * c + 409 * e + 128) >> 8).clamp(0, 255) as u8;
            let g = ((298 * c - 100 * d - 208 * e + 128) >> 8).clamp(0, 255) as u8;
            let b = ((298 * c + 516 * d + 128) >> 8).clamp(0, 255) as u8;

            let idx = (row * w + col) * 4;
            rgba[idx] = r;
            rgba[idx + 1] = g;
            rgba[idx + 2] = b;
            rgba[idx + 3] = 255;
        }
    }

    rgba
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::video::{capture::rgb_to_i420, vp8::Vp8Encoder};

    // ── i420_to_rgba ──────────────────────────────────────────────────────────

    #[test]
    fn rgba_output_size() {
        let frame = VideoFrame::new_black(8, 6);
        assert_eq!(i420_to_rgba(&frame).len(), 8 * 6 * 4);
    }

    #[test]
    fn rgba_black() {
        // new_black sets Y=16, U=V=128 → RGB should be (0,0,0)
        let frame = VideoFrame::new_black(4, 4);
        let rgba = i420_to_rgba(&frame);
        for chunk in rgba.chunks_exact(4) {
            assert_eq!(chunk[0], 0, "R for black");
            assert_eq!(chunk[1], 0, "G for black");
            assert_eq!(chunk[2], 0, "B for black");
            assert_eq!(chunk[3], 255, "A must be 255");
        }
    }

    #[test]
    fn rgba_white() {
        // Build a white I420 frame: Y=235, U=V=128
        let w = 4u32;
        let h = 4u32;
        let y_size = (w * h) as usize;
        let uv_size = ((w / 2) * (h / 2)) as usize;
        let mut data = vec![0u8; y_size + 2 * uv_size];
        data[..y_size].fill(235);
        data[y_size..].fill(128);
        let frame = VideoFrame { width: w, height: h, data };

        let rgba = i420_to_rgba(&frame);
        for chunk in rgba.chunks_exact(4) {
            assert_eq!(chunk[0], 255, "R for white");
            assert_eq!(chunk[1], 255, "G for white");
            assert_eq!(chunk[2], 255, "B for white");
            assert_eq!(chunk[3], 255, "A must be 255");
        }
    }

    #[test]
    fn rgb_i420_rgba_roundtrip_grey() {
        // A mid-grey image should survive the rgb→i420→rgba roundtrip approximately.
        let w = 8u32;
        let h = 8u32;
        let grey: u8 = 128;
        let rgb = vec![grey; (w * h * 3) as usize];
        let i420 = rgb_to_i420(&rgb, w, h);
        let frame = VideoFrame { width: w, height: h, data: i420 };
        let rgba = i420_to_rgba(&frame);

        for chunk in rgba.chunks_exact(4) {
            let r = chunk[0] as i32;
            let g = chunk[1] as i32;
            let b = chunk[2] as i32;
            // Allow ±8 for rounding losses in the BT.601 integer approximation.
            assert!((r - grey as i32).abs() <= 8, "R off by {}", (r - grey as i32).abs());
            assert!((g - grey as i32).abs() <= 8, "G off by {}", (g - grey as i32).abs());
            assert!((b - grey as i32).abs() <= 8, "B off by {}", (b - grey as i32).abs());
        }
    }

    // ── MediaTrackVp8Decoder ──────────────────────────────────────────────────

    #[test]
    fn rejects_non_vp8_track() {
        use crate::{
            codec::{opus::OpusChannels, Codec},
            rtc::{MediaTrack, TrackKind},
        };
        let (_, rx) = tokio::sync::broadcast::channel(4);
        let track = MediaTrack::new(rx, Codec::Opus { channels: OpusChannels::Stereo }, TrackKind::Audio);
        assert!(MediaTrackVp8Decoder::new(track).is_err());
    }

    #[tokio::test]
    async fn decoder_yields_frames() {
        let width = 64u32;
        let height = 64u32;
        let fps = 30u32;

        let (mut encoder, track) = Vp8Encoder::new(width, height, fps, 200, 32).unwrap();
        let mut decoder = MediaTrackVp8Decoder::new(track).unwrap();

        let frame = VideoFrame::new_black(width, height);
        // Push enough frames for the VP8 encoder to emit at least one keyframe.
        for _ in 0..5 {
            encoder.push_frame(&frame).unwrap();
        }

        // Give the broadcast channel time to buffer.
        tokio::time::sleep(Duration::from_millis(20)).await;

        let mut got_frame = false;
        for _ in 0..20 {
            match decoder.next_frame().unwrap() {
                ControlFlow::Continue(Some(f)) => {
                    assert_eq!(f.width, width);
                    assert_eq!(f.height, height);
                    got_frame = true;
                    break;
                }
                ControlFlow::Continue(None) => {}
                ControlFlow::Break(()) => break,
            }
        }
        assert!(got_frame, "decoder should have yielded at least one frame");
    }

    #[tokio::test]
    async fn decoder_breaks_when_track_closed() {
        let (mut encoder, track) = Vp8Encoder::new(32, 32, 30, 200, 32).unwrap();
        let mut decoder = MediaTrackVp8Decoder::new(track).unwrap();

        // Close the encoder/track immediately by dropping.
        let frame = VideoFrame::new_black(32, 32);
        for _ in 0..3 {
            encoder.push_frame(&frame).unwrap();
        }
        drop(encoder);

        // Drain all frames then expect Break.
        let mut saw_break = false;
        for _ in 0..100 {
            match decoder.next_frame().unwrap() {
                ControlFlow::Break(()) => {
                    saw_break = true;
                    break;
                }
                _ => {}
            }
        }
        assert!(saw_break, "decoder must signal Break after track closes");
    }
}
