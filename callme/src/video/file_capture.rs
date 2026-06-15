use std::{
    io::Read,
    path::PathBuf,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, info, warn};

use crate::{
    rtc::MediaTrack,
    video::{VideoFrame, VideoSink, vp8::Vp8Encoder},
};

/// Configuration for reading a video file.
pub struct FileConfig {
    /// Path to the video file (any format supported by ffmpeg).
    pub path: PathBuf,
    /// Restart playback from the beginning when the file ends.
    pub looping: bool,
}

/// Reads frames from a video file and fans them out to registered [`VideoSink`]s.
///
/// Uses `ffprobe` to query stream metadata and `ffmpeg` to decode frames to raw
/// I420 (YUV 4:2:0 planar).  Both tools must be available on `PATH` at runtime.
///
/// Analogous to [`super::capture::VideoCapture`] but driven by a file rather
/// than a camera device.
pub struct FileVideoCapture {
    sink_sender: mpsc::Sender<Box<dyn VideoSink>>,
    width: u32,
    height: u32,
    fps: u32,
}

impl FileVideoCapture {
    /// Open the video file at `config.path` and start the decode loop.
    ///
    /// Returns an error if `ffprobe` is not found, the file is missing, or the
    /// file has no video streams.
    pub async fn build(config: FileConfig) -> Result<Self> {
        let (width, height, fps_f64) = probe_video(&config.path)?;
        let fps = fps_f64
            .round()
            .max(1.0) as u32;

        let (frame_tx, frame_rx) = mpsc::channel(4);
        let (sink_tx, sink_rx) = mpsc::channel(16);
        let (init_tx, init_rx) = oneshot::channel::<Result<()>>();

        std::thread::Builder::new()
            .name("file-dispatch".into())
            .spawn(move || super::capture::dispatch_loop(frame_rx, sink_rx))
            .context("failed to spawn file dispatch thread")?;

        let path = config
            .path
            .clone();
        let looping = config.looping;
        std::thread::Builder::new()
            .name("file-capture".into())
            .spawn(move || {
                init_tx
                    .send(Ok(()))
                    .ok();
                file_reader_loop(path, width, height, fps_f64, looping, frame_tx);
            })
            .context("failed to spawn file reader thread")?;

        init_rx.await??;

        Ok(Self {
            sink_sender: sink_tx,
            width,
            height,
            fps,
        })
    }

    /// Register a new sink. Frames are pushed to it until it signals
    /// [`std::ops::ControlFlow::Break`] or returns an error.
    pub async fn add_sink(&self, sink: impl VideoSink) -> Result<()> {
        self.sink_sender
            .send(Box::new(sink))
            .await
            .map_err(|_| anyhow!("file capture loop closed"))
    }

    /// Create a VP8-encoded [`MediaTrack`] backed by this capture.
    pub async fn create_vp8_track(&self, bitrate_kbps: u32) -> Result<MediaTrack> {
        let (encoder, track) =
            Vp8Encoder::new(self.width, self.height, self.fps, bitrate_kbps, 16)?;
        self.add_sink(encoder)
            .await?;
        Ok(track)
    }
}

// ── file reader ───────────────────────────────────────────────────────────────

fn file_reader_loop(
    path: PathBuf,
    width: u32,
    height: u32,
    fps: f64,
    looping: bool,
    frame_tx: mpsc::Sender<VideoFrame>,
) {
    let frame_size = (width as usize * height as usize * 3) / 2;
    let frame_duration = Duration::from_secs_f64(1.0 / fps.max(0.001));

    let path_str = match path.to_str() {
        Some(s) => s.to_owned(),
        None => {
            warn!("video file path is not valid UTF-8");
            return;
        }
    };

    loop {
        let mut child = match Command::new("ffmpeg")
            .args([
                "-nostdin", "-i", &path_str, "-f", "rawvideo", "-pix_fmt", "yuv420p", "pipe:1",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                warn!("failed to spawn ffmpeg: {e}");
                return;
            }
        };

        let mut stdout = child
            .stdout
            .take()
            .expect("stdout was piped");
        let mut buf = vec![0u8; frame_size];
        let mut next_frame_time = Instant::now() + frame_duration;

        loop {
            match stdout.read_exact(&mut buf) {
                Ok(()) => {
                    let frame = VideoFrame {
                        width,
                        height,
                        data: buf.to_vec(),
                    };
                    if frame_tx
                        .blocking_send(frame)
                        .is_err()
                    {
                        debug!("file capture: frame channel closed");
                        child
                            .kill()
                            .ok();
                        return;
                    }
                    // Pace frames to the source frame rate.
                    let now = Instant::now();
                    if now < next_frame_time {
                        std::thread::sleep(next_frame_time - now);
                    }
                    next_frame_time += frame_duration;
                }
                Err(e) => {
                    if e.kind() != std::io::ErrorKind::UnexpectedEof {
                        warn!("ffmpeg stdout read error: {e}");
                    }
                    break;
                }
            }
        }

        child
            .wait()
            .ok();
        info!("file video: end of file");

        if !looping {
            break;
        }
        info!("file video: looping");
    }
    info!("file capture loop ended");
}

// ── ffprobe ───────────────────────────────────────────────────────────────────

/// Query the first video stream of `path` via `ffprobe`.
///
/// Returns `(width, height, fps)` on success.
fn probe_video(path: &PathBuf) -> Result<(u32, u32, f64)> {
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "quiet",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=width,height,r_frame_rate",
            "-print_format",
            "compact=s=|",
        ])
        .arg(path)
        .output()
        .context("failed to run ffprobe — is ffmpeg installed?")?;

    if !output
        .status
        .success()
    {
        bail!("ffprobe failed for {:?}", path);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout
        .lines()
        .next()
        .ok_or_else(|| anyhow!("no video stream found in {:?}", path))?;

    // Output format: "stream|width=NNN|height=NNN|r_frame_rate=NUM/DEN"
    let mut width = None;
    let mut height = None;
    let mut fps = None;

    for field in line.split('|') {
        if let Some(v) = field.strip_prefix("width=") {
            width = Some(
                v.parse::<u32>()
                    .context("invalid width")?,
            );
        } else if let Some(v) = field.strip_prefix("height=") {
            height = Some(
                v.parse::<u32>()
                    .context("invalid height")?,
            );
        } else if let Some(v) = field.strip_prefix("r_frame_rate=") {
            fps = Some(parse_fps(v).ok_or_else(|| anyhow!("invalid fps: {v}"))?);
        }
    }

    Ok((
        width.ok_or_else(|| anyhow!("ffprobe did not report video width"))?,
        height.ok_or_else(|| anyhow!("ffprobe did not report video height"))?,
        fps.ok_or_else(|| anyhow!("ffprobe did not report video frame rate"))?,
    ))
}

/// Parse an ffprobe rational frame rate string ("30/1", "30000/1001", "24").
fn parse_fps(s: &str) -> Option<f64> {
    if let Some((num, den)) = s.split_once('/') {
        let n: f64 = num
            .parse()
            .ok()?;
        let d: f64 = den
            .parse()
            .ok()?;
        (d > 0.0).then(|| n / d)
    } else {
        s.parse()
            .ok()
    }
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_fps_integer_ratio() {
        assert_eq!(parse_fps("30/1"), Some(30.0));
    }

    #[test]
    fn parse_fps_ntsc() {
        let fps = parse_fps("30000/1001").unwrap();
        assert!((fps - 29.97).abs() < 0.01, "expected ~29.97, got {fps}");
    }

    #[test]
    fn parse_fps_plain() {
        assert_eq!(parse_fps("24"), Some(24.0));
    }

    #[test]
    fn parse_fps_zero_denominator() {
        assert_eq!(parse_fps("30/0"), None);
    }

    #[test]
    fn parse_fps_garbage() {
        assert_eq!(parse_fps("not/a/fps"), None);
    }

    /// Simulates the ffprobe compact output format to verify field parsing.
    #[test]
    fn parse_probe_fields() {
        let line = "stream|width=1920|height=1080|r_frame_rate=30000/1001";
        let mut width = None;
        let mut height = None;
        let mut fps = None;
        for field in line.split('|') {
            if let Some(v) = field.strip_prefix("width=") {
                width = Some(
                    v.parse::<u32>()
                        .unwrap(),
                );
            } else if let Some(v) = field.strip_prefix("height=") {
                height = Some(
                    v.parse::<u32>()
                        .unwrap(),
                );
            } else if let Some(v) = field.strip_prefix("r_frame_rate=") {
                fps = Some(parse_fps(v).unwrap());
            }
        }
        assert_eq!(width, Some(1920));
        assert_eq!(height, Some(1080));
        assert!((fps.unwrap() - 29.97).abs() < 0.01);
    }

    /// End-to-end test: create a synthetic MP4 with ffmpeg then decode it.
    ///
    /// Skipped by default because it requires `ffmpeg` on `PATH`.
    /// Run manually with: `cargo test -- --ignored file_capture_produces_frames`
    #[tokio::test]
    #[ignore = "requires ffmpeg in PATH"]
    async fn file_capture_produces_frames() {
        use std::ops::ControlFlow;
        use std::sync::{Arc, Mutex};

        let tmp = std::env::temp_dir().join("callme_test_video.mp4");

        // Generate a tiny synthetic video: 4 frames of 64×64 @ 10 fps.
        let status = Command::new("ffmpeg")
            .args([
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=64x64:rate=10",
                "-t",
                "0.4",
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv420p",
            ])
            .arg(&tmp)
            .status()
            .expect("failed to run ffmpeg");
        assert!(status.success(), "ffmpeg failed to create test video");

        struct Collect(Arc<Mutex<Vec<VideoFrame>>>);
        impl VideoSink for Collect {
            fn push_frame(&mut self, frame: &VideoFrame) -> anyhow::Result<ControlFlow<(), ()>> {
                let mut v = self
                    .0
                    .lock()
                    .unwrap();
                v.push(frame.clone());
                if v.len() >= 3 {
                    Ok(ControlFlow::Break(()))
                } else {
                    Ok(ControlFlow::Continue(()))
                }
            }
        }

        let frames = Arc::new(Mutex::new(Vec::new()));
        let capture = FileVideoCapture::build(FileConfig {
            path: tmp.clone(),
            looping: false,
        })
        .await
        .expect("FileVideoCapture::build failed");

        capture
            .add_sink(Collect(frames.clone()))
            .await
            .unwrap();

        // Wait up to 5 s for 3 frames.
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        while tokio::time::Instant::now() < deadline {
            if frames
                .lock()
                .unwrap()
                .len()
                >= 3
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }

        let v = frames
            .lock()
            .unwrap();
        assert!(v.len() >= 3, "expected at least 3 frames, got {}", v.len());
        for f in v.iter() {
            assert_eq!(f.width, 64);
            assert_eq!(f.height, 64);
            assert_eq!(
                f.data
                    .len(),
                64 * 64 * 3 / 2
            );
        }

        std::fs::remove_file(tmp).ok();
    }
}
