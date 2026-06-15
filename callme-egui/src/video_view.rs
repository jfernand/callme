use std::ops::ControlFlow;
use std::time::Duration;

use anyhow::Result;
use callme::{
    rtc::MediaTrack,
    video::{render::{i420_to_rgba, MediaTrackVp8Decoder}, VideoSource},
};
use egui::{ColorImage, TextureHandle, TextureOptions, Ui};

/// An egui widget that decodes a VP8 [`MediaTrack`] and displays each frame
/// as an updating texture.
pub struct VideoView {
    decoder: MediaTrackVp8Decoder,
    texture: Option<TextureHandle>,
    closed: bool,
}

impl VideoView {
    pub fn new(track: MediaTrack) -> Result<Self> {
        Ok(Self {
            decoder: MediaTrackVp8Decoder::new(track)?,
            texture: None,
            closed: false,
        })
    }

    /// Returns `true` as long as the track is still open.
    pub fn is_open(&self) -> bool {
        !self.closed
    }

    /// Drain all pending decoded frames, upload the latest as a texture, and
    /// render it into `ui`.  Call this every frame from [`eframe::App::update`].
    pub fn ui(&mut self, ui: &mut Ui) {
        self.poll_frames(ui.ctx());

        if let Some(texture) = &self.texture {
            let size = texture.size_vec2();
            let available = ui.available_size();
            // Scale to fit the available area while keeping the aspect ratio.
            let scale = (available.x / size.x).min(available.y / size.y).min(1.0);
            let display_size = size * scale;
            ui.add(egui::Image::from_texture((texture.id(), display_size)));
        } else if self.closed {
            ui.label("Video stream ended.");
        } else {
            ui.label("Waiting for video…");
        }

        // Request repaints at ~30 fps while the stream is open.
        if !self.closed {
            ui.ctx().request_repaint_after(Duration::from_millis(33));
        }
    }

    fn poll_frames(&mut self, ctx: &egui::Context) {
        if self.closed {
            return;
        }

        let mut latest = None;
        loop {
            match self.decoder.next_frame() {
                Ok(ControlFlow::Continue(Some(frame))) => latest = Some(frame),
                Ok(ControlFlow::Continue(None)) => break,
                Ok(ControlFlow::Break(())) => {
                    self.closed = true;
                    break;
                }
                Err(_) => {
                    self.closed = true;
                    break;
                }
            }
        }

        if let Some(frame) = latest {
            let rgba = i420_to_rgba(&frame);
            let image = ColorImage::from_rgba_unmultiplied(
                [frame.width as usize, frame.height as usize],
                &rgba,
            );
            match &mut self.texture {
                Some(tex) => tex.set(image, TextureOptions::default()),
                None => {
                    self.texture = Some(ctx.load_texture("video-frame", image, TextureOptions::default()));
                }
            }
        }
    }
}
