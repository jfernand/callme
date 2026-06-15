use std::ops::ControlFlow;

use anyhow::{anyhow, Context, Result};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, info, warn};

use crate::{
    rtc::MediaTrack,
    video::{VideoFrame, VideoSink, vp8::Vp8Encoder},
};

/// Configuration for opening a camera device.
#[derive(Debug, Clone)]
pub struct CameraConfig {
    /// Platform camera index (0 = first camera).
    pub index: u32,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

impl Default for CameraConfig {
    fn default() -> Self {
        CameraConfig {
            index: 0,
            width: 640,
            height: 480,
            fps: 30,
        }
    }
}

/// Captures frames from a camera and fans them out to registered [`VideoSink`]s.
///
/// Analogous to [`crate::audio::AudioCapture`] for audio.
pub struct VideoCapture {
    sink_sender: mpsc::Sender<Box<dyn VideoSink>>,
    width: u32,
    height: u32,
    fps: u32,
}

impl VideoCapture {
    /// Open the camera described by `config` and start the capture loop.
    ///
    /// Returns an error immediately if the camera cannot be opened.
    pub async fn build(config: CameraConfig) -> Result<Self> {
        let width = config.width;
        let height = config.height;
        let fps = config.fps;

        let (frame_tx, frame_rx) = mpsc::channel(4);
        let (sink_tx, sink_rx) = mpsc::channel(16);
        let (init_tx, init_rx) = oneshot::channel();

        // Dispatch loop: receives frames from the camera thread and pushes them to sinks.
        // Started first so it is ready before frames arrive.
        std::thread::Builder::new()
            .name("video-dispatch".into())
            .spawn(move || dispatch_loop(frame_rx, sink_rx))
            .context("failed to spawn video dispatch thread")?;

        // Camera thread: opens the hardware device, captures frames, and sends them.
        std::thread::Builder::new()
            .name("video-capture".into())
            .spawn(move || match open_camera(&config) {
                Ok(mut cam) => {
                    init_tx.send(Ok(())).ok();
                    camera_capture_loop(&mut cam, frame_tx);
                }
                Err(e) => {
                    init_tx.send(Err(e)).ok();
                }
            })
            .context("failed to spawn video capture thread")?;

        init_rx.await??;

        Ok(Self { sink_sender: sink_tx, width, height, fps })
    }

    /// Register a new sink. Frames are pushed to it until it signals
    /// [`ControlFlow::Break`] or returns an error.
    pub async fn add_sink(&self, sink: impl VideoSink) -> Result<()> {
        self.sink_sender
            .send(Box::new(sink))
            .await
            .map_err(|_| anyhow!("video capture loop closed"))
    }

    /// Create a VP8-encoded [`MediaTrack`] backed by this capture.
    pub async fn create_vp8_track(&self, bitrate_kbps: u32) -> Result<MediaTrack> {
        let (encoder, track) =
            Vp8Encoder::new(self.width, self.height, self.fps, bitrate_kbps, 16)?;
        self.add_sink(encoder).await?;
        Ok(track)
    }

    /// Build a `VideoCapture` driven by an external frame channel instead of a
    /// real camera.  Only compiled for tests.
    #[cfg(test)]
    pub(crate) fn new_for_test(
        frame_rx: mpsc::Receiver<VideoFrame>,
        width: u32,
        height: u32,
        fps: u32,
    ) -> Self {
        let (sink_tx, sink_rx) = mpsc::channel(16);
        std::thread::Builder::new()
            .name("video-dispatch-test".into())
            .spawn(move || dispatch_loop(frame_rx, sink_rx))
            .expect("failed to spawn test dispatch thread");
        Self { sink_sender: sink_tx, width, height, fps }
    }
}

// ── hardware layer ────────────────────────────────────────────────────────────

fn open_camera(config: &CameraConfig) -> Result<nokhwa::Camera> {
    use nokhwa::{
        pixel_format::RgbFormat,
        utils::{CameraFormat, CameraIndex, FrameFormat, RequestedFormat, RequestedFormatType, Resolution},
    };

    let requested = RequestedFormat::new::<RgbFormat>(RequestedFormatType::Closest(
        CameraFormat::new(
            Resolution::new(config.width, config.height),
            FrameFormat::YUYV,
            config.fps,
        ),
    ));
    let mut camera = nokhwa::Camera::new(CameraIndex::Index(config.index), requested)
        .context("failed to open camera")?;
    camera.open_stream().context("failed to open camera stream")?;
    info!(
        width = config.width,
        height = config.height,
        fps = config.fps,
        "camera stream opened"
    );
    Ok(camera)
}

fn camera_capture_loop(camera: &mut nokhwa::Camera, frame_tx: mpsc::Sender<VideoFrame>) {
    use nokhwa::pixel_format::RgbFormat;

    loop {
        let buffer = match camera.frame() {
            Ok(b) => b,
            Err(e) => {
                warn!("camera frame error: {e}");
                break;
            }
        };
        let rgb = match buffer.decode_image::<RgbFormat>() {
            Ok(r) => r,
            Err(e) => {
                warn!("failed to decode camera frame: {e}");
                continue;
            }
        };
        let (w, h) = (rgb.width(), rgb.height());
        let frame = VideoFrame {
            width: w,
            height: h,
            data: rgb_to_i420(rgb.as_raw(), w, h),
        };
        if frame_tx.blocking_send(frame).is_err() {
            debug!("frame channel closed, stopping camera capture");
            break;
        }
    }
    info!("camera capture loop ended");
}

// ── dispatch loop ─────────────────────────────────────────────────────────────

fn dispatch_loop(
    mut frame_rx: mpsc::Receiver<VideoFrame>,
    mut sink_rx: mpsc::Receiver<Box<dyn VideoSink>>,
) {
    info!("video dispatch loop start");
    let mut sinks: Vec<Box<dyn VideoSink>> = Vec::new();

    loop {
        // Accept any newly registered sinks without blocking.
        loop {
            match sink_rx.try_recv() {
                Ok(sink) => {
                    info!("video sink added (total {})", sinks.len() + 1);
                    sinks.push(sink);
                }
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    info!("video dispatch: sink channel closed");
                    return;
                }
            }
        }

        let frame = match frame_rx.blocking_recv() {
            Some(f) => f,
            None => {
                info!("video frame channel closed");
                break;
            }
        };

        sinks.retain_mut(|sink| match sink.push_frame(&frame) {
            Ok(ControlFlow::Continue(())) => true,
            Ok(ControlFlow::Break(())) => {
                debug!("video sink signalled close");
                false
            }
            Err(e) => {
                warn!("video sink error: {e}");
                false
            }
        });
    }
    info!("video dispatch loop end");
}

// ── colour conversion ─────────────────────────────────────────────────────────

/// Convert packed RGB24 to planar I420 (YCbCr 4:2:0, limited range BT.601).
///
/// Input: `rgb` is `width * height * 3` bytes, RGB interleaved.
/// Output: `width * height * 3 / 2` bytes — Y plane then U plane then V plane.
pub fn rgb_to_i420(rgb: &[u8], width: u32, height: u32) -> Vec<u8> {
    let w = width as usize;
    let h = height as usize;
    let y_size = w * h;
    let uv_rows = h / 2;
    let uv_cols = w / 2;
    let uv_size = uv_rows * uv_cols;
    let mut out = vec![0u8; y_size + 2 * uv_size];

    // Y plane: one luma sample per pixel.
    for row in 0..h {
        for col in 0..w {
            let base = (row * w + col) * 3;
            let r = rgb[base] as i32;
            let g = rgb[base + 1] as i32;
            let b = rgb[base + 2] as i32;
            // BT.601 limited range (Y∈[16,235]).
            let y = ((66 * r + 129 * g + 25 * b + 128) >> 8) + 16;
            out[row * w + col] = y.clamp(0, 255) as u8;
        }
    }

    // UV planes: average each 2×2 pixel block before converting to chroma.
    // Averaging reduces colour fringing on edges compared to top-left-only sampling.
    for br in 0..uv_rows {
        for bc in 0..uv_cols {
            let mut r_sum = 0i32;
            let mut g_sum = 0i32;
            let mut b_sum = 0i32;
            for dr in 0..2usize {
                for dc in 0..2usize {
                    // Clamp handles odd-dimension inputs gracefully.
                    let row = (br * 2 + dr).min(h - 1);
                    let col = (bc * 2 + dc).min(w - 1);
                    let base = (row * w + col) * 3;
                    r_sum += rgb[base] as i32;
                    g_sum += rgb[base + 1] as i32;
                    b_sum += rgb[base + 2] as i32;
                }
            }
            let r = r_sum / 4;
            let g = g_sum / 4;
            let b = b_sum / 4;
            // BT.601 limited range (Cb/Cr∈[16,240]).
            let u = ((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128;
            let v = ((112 * r - 94 * g - 18 * b + 128) >> 8) + 128;
            let uv_idx = br * uv_cols + bc;
            out[y_size + uv_idx] = u.clamp(0, 255) as u8;
            out[y_size + uv_size + uv_idx] = v.clamp(0, 255) as u8;
        }
    }

    out
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::{ops::ControlFlow, time::Duration};

    use anyhow::Result;
    use tokio::sync::mpsc;

    use super::*;
    use crate::{codec::Codec, video::VideoFrame};

    // ── rgb_to_i420 ───────────────────────────────────────────────────────────

    #[test]
    fn i420_output_size() {
        let out = rgb_to_i420(&vec![0u8; 8 * 6 * 3], 8, 6);
        // I420 = Y(8*6) + U(4*3) + V(4*3) = 48 + 12 + 12 = 72 = 8*6*3/2
        assert_eq!(out.len(), 8 * 6 * 3 / 2);
    }

    #[test]
    fn i420_minimum_2x2_frame() {
        // Smallest valid I420 block: 2×2 pixels.
        let rgb = vec![128u8; 2 * 2 * 3];
        let out = rgb_to_i420(&rgb, 2, 2);
        assert_eq!(out.len(), 2 * 2 * 3 / 2, "output size");
        // All values should be within I420 range.
        for &b in &out {
            assert!(b <= 240, "I420 value {b} out of range");
        }
    }

    #[test]
    fn i420_black() {
        // R=G=B=0  →  Y=16, U=128, V=128  (BT.601 limited range)
        let out = rgb_to_i420(&vec![0u8; 4 * 4 * 3], 4, 4);
        let y_size = 16usize;
        let uv_size = 4usize;
        for &y in &out[..y_size] {
            assert_eq!(y, 16, "Y for black");
        }
        for &u in &out[y_size..y_size + uv_size] {
            assert_eq!(u, 128, "U for black");
        }
        for &v in &out[y_size + uv_size..] {
            assert_eq!(v, 128, "V for black");
        }
    }

    #[test]
    fn i420_white() {
        // R=G=B=255  →  Y=235, U=128, V=128
        let out = rgb_to_i420(&vec![255u8; 4 * 4 * 3], 4, 4);
        let y_size = 16usize;
        let uv_size = 4usize;
        for &y in &out[..y_size] {
            assert_eq!(y, 235, "Y for white");
        }
        for &u in &out[y_size..y_size + uv_size] {
            assert_eq!(u, 128, "U for white");
        }
        for &v in &out[y_size + uv_size..] {
            assert_eq!(v, 128, "V for white");
        }
    }

    #[test]
    fn i420_pure_red() {
        // R=255, G=0, B=0 uniform image: all 4 pixels in each 2×2 block are the
        // same colour, so averaging doesn't change the chroma result.
        // BT.601: Y=82, Cb=90, Cr=240
        let mut rgb = vec![0u8; 4 * 4 * 3];
        for p in rgb.chunks_exact_mut(3) {
            p[0] = 255;
        }
        let out = rgb_to_i420(&rgb, 4, 4);
        let y_size = 16usize;
        let uv_size = 4usize;
        // ((66*255 + 128) >> 8) + 16 = (16958 >> 8) + 16 = 66 + 16 = 82
        for &y in &out[..y_size] {
            assert_eq!(y, 82, "Y for red");
        }
        for &u in &out[y_size..y_size + uv_size] {
            assert_eq!(u, 90, "U (Cb) for red");
        }
        for &v in &out[y_size + uv_size..] {
            assert_eq!(v, 240, "V (Cr) for red");
        }
    }

    // ── dispatch loop ─────────────────────────────────────────────────────────

    /// A sink that forwards every received frame to an async channel and breaks
    /// after `limit` frames.  Using a channel lets tests wait for exact frame
    /// counts without sleeping.
    struct CollectSink {
        tx: tokio::sync::mpsc::UnboundedSender<VideoFrame>,
        limit: usize,
        count: usize,
    }

    impl VideoSink for CollectSink {
        fn push_frame(&mut self, frame: &VideoFrame) -> Result<ControlFlow<(), ()>> {
            self.count += 1;
            self.tx.send(frame.clone()).ok();
            if self.count >= self.limit {
                Ok(ControlFlow::Break(()))
            } else {
                Ok(ControlFlow::Continue(()))
            }
        }
    }

    #[tokio::test]
    async fn dispatch_delivers_frames() {
        let (frame_tx, frame_rx) = mpsc::channel(8);
        let capture = VideoCapture::new_for_test(frame_rx, 4, 4, 30);

        let (sink_tx, mut sink_rx) = tokio::sync::mpsc::unbounded_channel();
        capture
            .add_sink(CollectSink { tx: sink_tx, limit: 3, count: 0 })
            .await
            .unwrap();

        let frame = VideoFrame::new_black(4, 4);
        for _ in 0..3 {
            frame_tx.send(frame.clone()).await.unwrap();
        }

        for _ in 0..3 {
            tokio::time::timeout(Duration::from_secs(2), sink_rx.recv())
                .await
                .expect("timeout waiting for frame from dispatch")
                .expect("channel closed unexpectedly");
        }
    }

    #[tokio::test]
    async fn sink_removed_after_break() {
        let (frame_tx, frame_rx) = mpsc::channel(8);
        let capture = VideoCapture::new_for_test(frame_rx, 4, 4, 30);

        let (sink_tx, mut sink_rx) = tokio::sync::mpsc::unbounded_channel();
        // Sink breaks after 2 frames.
        capture
            .add_sink(CollectSink { tx: sink_tx, limit: 2, count: 0 })
            .await
            .unwrap();

        let frame = VideoFrame::new_black(4, 4);
        for _ in 0..5 {
            frame_tx.send(frame.clone()).await.unwrap();
        }

        // Wait for exactly 2 frames.
        for _ in 0..2 {
            tokio::time::timeout(Duration::from_secs(2), sink_rx.recv())
                .await
                .expect("timeout")
                .expect("channel closed");
        }

        // Give the dispatch loop time to process the remaining 3 frames, then
        // verify the (now-removed) sink received nothing extra.
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(sink_rx.try_recv().is_err(), "no frames expected after break");
    }

    #[tokio::test]
    async fn multiple_sinks_receive_same_frame() {
        let (frame_tx, frame_rx) = mpsc::channel(8);
        let capture = VideoCapture::new_for_test(frame_rx, 4, 4, 30);

        let (tx1, mut rx1) = tokio::sync::mpsc::unbounded_channel();
        let (tx2, mut rx2) = tokio::sync::mpsc::unbounded_channel();
        capture
            .add_sink(CollectSink { tx: tx1, limit: 99, count: 0 })
            .await
            .unwrap();
        capture
            .add_sink(CollectSink { tx: tx2, limit: 99, count: 0 })
            .await
            .unwrap();

        // Give the dispatch loop time to drain both sink registrations before
        // sending frames, so neither sink misses a frame due to a race.
        tokio::time::sleep(Duration::from_millis(20)).await;

        let frame = VideoFrame::new_black(4, 4);
        for _ in 0..3 {
            frame_tx.send(frame.clone()).await.unwrap();
        }

        for _ in 0..3 {
            tokio::time::timeout(Duration::from_secs(2), rx1.recv())
                .await.expect("timeout rx1").expect("closed");
            tokio::time::timeout(Duration::from_secs(2), rx2.recv())
                .await.expect("timeout rx2").expect("closed");
        }
    }

    // ── VP8 pipeline ──────────────────────────────────────────────────────────

    #[tokio::test]
    async fn create_vp8_track_codec_is_vp8() {
        let (_frame_tx, frame_rx) = mpsc::channel(8);
        let capture = VideoCapture::new_for_test(frame_rx, 128, 128, 30);
        let track = capture.create_vp8_track(500).await.unwrap();
        assert_eq!(track.codec(), Codec::Vp8);
    }

    #[tokio::test]
    async fn vp8_track_receives_encoded_frames() {
        let (frame_tx, frame_rx) = mpsc::channel(8);
        let capture = VideoCapture::new_for_test(frame_rx, 128, 128, 30);
        let mut track = capture.create_vp8_track(500).await.unwrap();

        let frame = VideoFrame::new_black(128, 128);
        for _ in 0..5 {
            frame_tx.send(frame.clone()).await.unwrap();
        }

        // Poll with a hard timeout rather than an arbitrary fixed sleep.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        let mut count = 0;
        while tokio::time::Instant::now() < deadline {
            while track.try_recv().is_ok() {
                count += 1;
            }
            if count > 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(count > 0, "VP8 track should have encoded frames");
    }
}
