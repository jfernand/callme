use std::ops::ControlFlow;

use anyhow::Result;

pub mod capture;
pub mod vp8;

/// A raw video frame in I420 (planar YUV 4:2:0) format.
///
/// Layout of `data`:
/// - Y plane: `width * height` bytes
/// - U plane: `(width/2) * (height/2)` bytes
/// - V plane: `(width/2) * (height/2)` bytes
#[derive(Debug, Clone)]
pub struct VideoFrame {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

impl VideoFrame {
    /// Create a black I420 frame (Y=16, U=V=128).
    pub fn new_black(width: u32, height: u32) -> Self {
        let y_size = (width * height) as usize;
        let uv_size = (width as usize / 2) * (height as usize / 2);
        let mut data = vec![0u8; y_size + 2 * uv_size];
        data[..y_size].fill(16);
        data[y_size..].fill(128);
        Self { width, height, data }
    }
}

/// Consumes video frames for encoding or rendering.
pub trait VideoSink: Send + 'static {
    fn push_frame(&mut self, frame: &VideoFrame) -> Result<ControlFlow<(), ()>>;
}

/// Produces decoded video frames.
pub trait VideoSource: Send + 'static {
    fn next_frame(&mut self) -> Result<ControlFlow<(), Option<VideoFrame>>>;
}
