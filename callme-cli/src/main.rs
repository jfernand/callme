use callme::{
    audio::{AudioConfig, AudioContext},
    net,
    rtc::{handle_connection_with_audio_context, RtcConnection, RtcProtocol},
    NodeId,
};
use clap::Parser;
use dialoguer::Confirm;
use iroh::protocol::Router;
use tokio::task::JoinSet;
use tracing::{error, info, warn};

#[cfg(feature = "video")]
use {
    callme::{
        rtc::TrackKind,
        video::{
            capture::{CameraConfig, VideoCapture},
            render::{i420_to_rgba, MediaTrackVp8Decoder},
            VideoFrame, VideoSource,
        },
    },
    crossterm::{cursor, execute, terminal},
    std::io::{stdout, Write},
    std::ops::ControlFlow,
    std::sync::mpsc,
    std::time::Duration,
};

// (video_enabled, camera_index, bitrate_kbps, ascii_mode)
type VideoCfg = (bool, u32, u32, bool);

#[derive(Parser, Debug)]
#[command(about = "Call me iroh", long_about = None)]
struct Args {
    /// The audio input device to use.
    #[arg(short, long)]
    input_device: Option<String>,
    /// The audio output device to use.
    #[arg(short, long)]
    output_device: Option<String>,
    /// If set, audio processing and echo cancellation will be disabled.
    #[arg(long)]
    disable_processing: bool,
    /// Enable video: capture local camera and display remote video.
    #[cfg(feature = "video")]
    #[arg(long)]
    video: bool,
    /// Camera device index (default: 0).
    #[cfg(feature = "video")]
    #[arg(long, default_value = "0")]
    camera: u32,
    /// VP8 encode bitrate in kbps for the outgoing video track (default: 500).
    #[cfg(feature = "video")]
    #[arg(long, default_value = "500")]
    video_bitrate: u32,
    /// Render remote video as ASCII art in the terminal instead of opening a pixel window.
    #[cfg(feature = "video")]
    #[arg(long)]
    ascii: bool,
    #[clap(subcommand)]
    command: Command,
}

impl Args {
    fn video_cfg(&self) -> VideoCfg {
        #[cfg(feature = "video")]
        return (self.video, self.camera, self.video_bitrate, self.ascii);
        #[cfg(not(feature = "video"))]
        (false, 0, 500, false)
    }
}

#[derive(Debug, Parser)]
enum Command {
    /// Accept calls from remote nodes.
    Accept {
        /// Accept more than one call.
        #[clap(long)]
        many: bool,
        /// Auto-accept calls without confirmation.
        #[clap(long)]
        auto: bool,
    },
    /// Make calls to remote nodes.
    Connect { node_id: Vec<NodeId> },
    /// Create a debug feedback loop through an in-memory channel.
    Feedback { mode: Option<FeedbackMode> },
    /// List the available audio devices.
    ListDevices,
}

#[derive(Debug, Clone, clap::ValueEnum, Default)]
enum FeedbackMode {
    #[default]
    Raw,
    Encoded,
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let args = Args::parse();
    let audio_config = AudioConfig {
        input_device: args.input_device.clone(),
        output_device: args.output_device.clone(),
        processing_enabled: !args.disable_processing,
    };
    // Extract before the match so the command destructure can't invalidate it.
    let vcfg = args.video_cfg();

    let mut endpoint_shutdown = None;
    let fut = async {
        match args.command {
            Command::Accept { many, auto } => {
                let endpoint = net::bind_endpoint().await?;
                let proto = RtcProtocol::new(endpoint.clone());
                let _router = Router::builder(endpoint.clone())
                    .accept(RtcProtocol::ALPN, proto.clone())
                    .spawn()
                    .await?;

                endpoint_shutdown = Some(endpoint.clone());
                println!("our node id:\n{}", endpoint.node_id());

                let audio_ctx = AudioContext::new(audio_config).await?;

                while let Some(conn) = proto.accept().await? {
                    if !many {
                        handle_connection(audio_ctx, conn, vcfg).await;
                        break;
                    } else {
                        let peer = conn.transport().remote_node_id()?.fmt_short();
                        let accept =
                            auto || confirm(format!("Incoming call from {peer}. Accept?")).await;
                        if accept {
                            n0_future::task::spawn(handle_connection(
                                audio_ctx.clone(),
                                conn,
                                vcfg,
                            ));
                        } else {
                            info!("reject connection from {peer}");
                            conn.transport().close(0u32.into(), b"bye");
                        }
                    }
                }
            }
            Command::Connect { node_id } => {
                let endpoint = net::bind_endpoint().await?;
                endpoint_shutdown = Some(endpoint.clone());

                let proto = RtcProtocol::new(endpoint);
                let audio_ctx = AudioContext::new(audio_config).await?;

                let mut join_set = JoinSet::new();
                for node_id in node_id {
                    info!("connecting to {}", node_id.fmt_short());
                    let audio_ctx = audio_ctx.clone();
                    let proto = proto.clone();
                    join_set.spawn(async move {
                        let fut = async {
                            let conn = proto.connect(node_id).await?;
                            info!("established connection to {}", node_id.fmt_short());
                            handle_connection(audio_ctx, conn, vcfg).await;
                            anyhow::Ok(())
                        };
                        (node_id, fut.await)
                    });
                }

                while let Some(res) = join_set.join_next().await {
                    let (node_id, res) = res.expect("task panicked");
                    if let Err(err) = res {
                        warn!("failed to connect to {}: {err:?}", node_id.fmt_short())
                    }
                }
            }
            Command::Feedback { mode } => {
                let ctx = AudioContext::new(audio_config).await?;
                let mode = mode.unwrap_or_default();
                println!("start feedback loop for 5 seconds (mode {mode:?}");
                match mode {
                    FeedbackMode::Raw => ctx.feedback_raw().await?,
                    FeedbackMode::Encoded => ctx.feedback_encoded().await?,
                }
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                println!("closing");
            }
            Command::ListDevices => {
                let devices = AudioContext::list_devices().await?;
                println!("{devices:?}");
            }
        }
        anyhow::Ok(())
    };

    tokio::select! {
        res = fut => res?,
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("shutting down");
            if let Some(endpoint) = endpoint_shutdown {
                endpoint.close().await;
            }
        }
    }
    Ok(())
}

async fn handle_connection(audio_ctx: AudioContext, conn: RtcConnection, vcfg: VideoCfg) {
    let peer = conn.transport().remote_node_id().unwrap().fmt_short();

    #[cfg(feature = "video")]
    if vcfg.0 {
        let (_, camera, bitrate, ascii) = vcfg;
        if let Err(err) =
            handle_connection_with_video(audio_ctx, conn, camera, bitrate, ascii).await
        {
            error!("connection from {peer} closed with error: {err:?}");
        } else {
            info!("connection from {peer} closed");
        }
        return;
    }

    if let Err(err) = handle_connection_with_audio_context(audio_ctx, conn).await {
        error!("connection from {peer} closed with error: {err:?}",)
    } else {
        info!("connection from {peer} closed")
    }
}

async fn confirm(msg: String) -> bool {
    tokio::task::spawn_blocking(move || Confirm::new().with_prompt(msg).interact().unwrap())
        .await
        .unwrap()
}

// ── video connection handler ──────────────────────────────────────────────────

#[cfg(feature = "video")]
async fn handle_connection_with_video(
    audio_ctx: AudioContext,
    conn: RtcConnection,
    camera_index: u32,
    video_bitrate_kbps: u32,
    ascii: bool,
) -> anyhow::Result<()> {
    // Frame channel: decoder tasks → display (capacity = 1 s at 30 fps).
    let (frame_tx, frame_rx) = mpsc::sync_channel::<VideoFrame>(30);

    // Try to open the camera and send a video track.
    // Store the capture handle so background threads stay alive for the call.
    let _capture = match VideoCapture::build(CameraConfig {
        index: camera_index,
        ..Default::default()
    })
    .await
    {
        Ok(capture) => {
            match capture.create_vp8_track(video_bitrate_kbps).await {
                Ok(track) => {
                    conn.send_track(track).await?;
                    info!("video track sent (camera {camera_index})");
                }
                Err(e) => warn!("failed to create VP8 track: {e}"),
            }
            Some(capture)
        }
        Err(e) => {
            warn!("failed to open camera {camera_index}: {e} — continuing without local video");
            None
        }
    };

    // Send audio track.
    let audio_track = audio_ctx.capture_track().await?;
    conn.send_track(audio_track).await?;
    info!("audio track sent");

    // Spawn the display on a blocking thread.  Waits for the first frame before
    // opening a window / entering the alternate screen.
    let display_task = if ascii {
        tokio::task::spawn_blocking(move || run_ascii_video(frame_rx))
    } else {
        tokio::task::spawn_blocking(move || run_video_window(frame_rx))
    };

    // Receive and route incoming tracks.
    let conn_result = async {
        while let Some(remote_track) = conn.recv_track().await? {
            match remote_track.kind() {
                TrackKind::Audio => {
                    audio_ctx.play_track(remote_track).await?;
                }
                TrackKind::Video => {
                    spawn_video_decoder(remote_track, frame_tx.clone());
                }
            }
        }
        anyhow::Ok(())
    };

    tokio::select! {
        res = conn_result => res?,
        _ = display_task => {},
    }

    Ok(())
}

/// Spawn a tokio task that decodes `track` and forwards frames to `tx`.
#[cfg(feature = "video")]
fn spawn_video_decoder(track: callme::rtc::MediaTrack, tx: mpsc::SyncSender<VideoFrame>) {
    tokio::spawn(async move {
        let mut decoder = match MediaTrackVp8Decoder::new(track) {
            Ok(d) => d,
            Err(e) => {
                warn!("failed to create VP8 decoder: {e}");
                return;
            }
        };
        loop {
            match decoder.next_frame() {
                Ok(ControlFlow::Continue(Some(frame))) => {
                    // Non-blocking: drop the frame if the display is falling behind.
                    tx.try_send(frame).ok();
                }
                Ok(ControlFlow::Continue(None)) => {
                    // No frame ready yet — yield so we don't busy-spin.
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
                Ok(ControlFlow::Break(())) => {
                    info!("remote video track closed");
                    break;
                }
                Err(e) => {
                    warn!("VP8 decode error: {e}");
                    break;
                }
            }
        }
    });
}

// ── minifb pixel window ───────────────────────────────────────────────────────

#[cfg(feature = "video")]
fn run_video_window(frame_rx: mpsc::Receiver<VideoFrame>) {
    use minifb::{Window, WindowOptions};

    // Wait for the first frame so we know the resolution before creating the window.
    let first = match frame_rx.recv_timeout(Duration::from_secs(30)) {
        Ok(f) => f,
        Err(mpsc::RecvTimeoutError::Timeout) => {
            info!("no remote video received within 30 s; not opening display window");
            return;
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => return,
    };

    let (mut win_w, mut win_h) = (first.width as usize, first.height as usize);
    let mut pixels = i420_to_minifb(&first);

    let mut window = match Window::new(
        "callme — remote video",
        win_w,
        win_h,
        WindowOptions { resize: true, ..Default::default() },
    ) {
        Ok(w) => w,
        Err(e) => {
            warn!("failed to open video window: {e}");
            return;
        }
    };

    #[allow(deprecated)]
    window.limit_update_rate(Some(Duration::from_millis(33)));

    let mut disconnected = false;
    while window.is_open() && !disconnected {
        let mut latest = None;
        loop {
            match frame_rx.try_recv() {
                Ok(frame) => latest = Some(frame),
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    disconnected = true;
                    break;
                }
            }
        }

        if let Some(frame) = latest {
            win_w = frame.width as usize;
            win_h = frame.height as usize;
            pixels = i420_to_minifb(&frame);
        }

        if window.update_with_buffer(&pixels, win_w, win_h).is_err() {
            break;
        }
    }

    info!("video window closed");
}

#[cfg(feature = "video")]
fn i420_to_minifb(frame: &VideoFrame) -> Vec<u32> {
    i420_to_rgba(frame)
        .chunks_exact(4)
        .map(|c| ((c[0] as u32) << 16) | ((c[1] as u32) << 8) | (c[2] as u32))
        .collect()
}

// ── ASCII terminal renderer ───────────────────────────────────────────────────

/// Render incoming video frames as ASCII art in the terminal's alternate screen.
///
/// Blocks until the frame channel disconnects or 30 s elapse with no first frame.
/// Intended to run inside [`tokio::task::spawn_blocking`].
#[cfg(feature = "video")]
fn run_ascii_video(frame_rx: mpsc::Receiver<VideoFrame>) {
    let mut out = stdout();

    // Enter the alternate screen buffer and hide the cursor so the rendering
    // doesn't interfere with the shell session.
    execute!(out, terminal::EnterAlternateScreen, cursor::Hide).ok();

    let result = ascii_render_loop(&frame_rx, &mut out);

    // Always restore the terminal, even on error.
    execute!(out, terminal::LeaveAlternateScreen, cursor::Show).ok();

    if let Err(e) = result {
        warn!("ASCII video render error: {e}");
    }
    info!("ASCII video display ended");
}

#[cfg(feature = "video")]
fn ascii_render_loop(
    frame_rx: &mpsc::Receiver<VideoFrame>,
    out: &mut impl Write,
) -> anyhow::Result<()> {
    // Wait for the first frame before entering the render loop.
    let first = match frame_rx.recv_timeout(Duration::from_secs(30)) {
        Ok(f) => f,
        Err(mpsc::RecvTimeoutError::Timeout) => {
            info!("no remote video received within 30 s; not rendering ASCII");
            return Ok(());
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
    };

    let mut current = first;

    loop {
        let (cols, rows) = terminal::size().unwrap_or((80, 24));

        // Render the current frame.
        let rendered = frame_to_ascii(&current, cols, rows);
        execute!(out, cursor::MoveTo(0, 0))?;
        out.write_all(rendered.as_bytes())?;
        out.flush()?;

        // Sleep one frame period, then drain the channel for the latest frame.
        std::thread::sleep(Duration::from_millis(33));

        let mut disconnected = false;
        loop {
            match frame_rx.try_recv() {
                Ok(f) => current = f,
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    disconnected = true;
                    break;
                }
            }
        }
        if disconnected {
            break;
        }
    }

    Ok(())
}

/// Convert an I420 [`VideoFrame`] to a block of ASCII text sized to fit `cols × rows`.
///
/// Uses the luma (Y) plane only; colour information is discarded.  The output
/// is scaled proportionally — with a 2:1 character-cell aspect ratio assumed —
/// so the image is letter-boxed rather than stretched.
#[cfg(feature = "video")]
fn frame_to_ascii(frame: &VideoFrame, cols: u16, rows: u16) -> String {
    // ASCII gradient from darkest to lightest.
    const CHARS: &[u8] = b" .,:;+*?%#@";
    // Assumed height-to-width ratio of a terminal character cell.
    // Most modern terminal fonts are close to 2:1.
    const CHAR_ASPECT: f32 = 2.0;

    let fw = frame.width as f32;
    let fh = frame.height as f32;

    // Reserve the bottom row so status text / the shell prompt isn't overwritten.
    let avail_cols = cols as f32;
    let avail_rows = rows.saturating_sub(1) as f32;

    // Scale uniformly so the frame fits the terminal area.
    // Terminal display area in virtual pixels: avail_cols × (avail_rows × CHAR_ASPECT).
    let scale = (avail_cols / fw).min(avail_rows * CHAR_ASPECT / fh);
    let disp_w = ((fw * scale) as usize).min(cols as usize);
    let disp_h = ((fh * scale / CHAR_ASPECT) as usize).min(rows.saturating_sub(1) as usize);

    // Centre the image horizontally.
    let left_pad = (cols as usize).saturating_sub(disp_w) / 2;
    let pad = " ".repeat(left_pad);

    let fw_u = frame.width as usize;
    let fh_u = frame.height as usize;
    let y_plane = &frame.data[..fw_u * fh_u];

    let mut out = String::with_capacity((left_pad + disp_w + 1) * disp_h);

    for row in 0..disp_h {
        out.push_str(&pad);
        for col in 0..disp_w {
            // Nearest-neighbour sample with half-pixel offset for better centering.
            let src_x = ((col * fw_u) + fw_u / 2) / disp_w.max(1);
            let src_y = ((row * fh_u) + fh_u / 2) / disp_h.max(1);
            let y = y_plane[src_y.min(fh_u - 1) * fw_u + src_x.min(fw_u - 1)];
            // Map Y ∈ [16, 235] (BT.601 limited range) to a character index.
            let n = (y.saturating_sub(16) as usize * (CHARS.len() - 1)) / 219;
            out.push(CHARS[n.min(CHARS.len() - 1)] as char);
        }
        out.push('\n');
    }

    out
}
