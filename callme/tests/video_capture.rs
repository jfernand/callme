/// Integration tests for the video capture pipeline.
///
/// These tests exercise the public API only and do not require camera hardware.
/// Tests that need a real camera device are marked `#[ignore]`.
#[cfg(feature = "video")]
mod tests {
    use std::ops::ControlFlow;

    use callme::video::{
        capture::{rgb_to_i420, CameraConfig},
        vp8::{Vp8Decoder, Vp8Encoder},
        VideoFrame, VideoSink,
    };

    // ── rgb_to_i420 public API ────────────────────────────────────────────────

    #[test]
    fn public_rgb_to_i420_output_size() {
        let width = 16u32;
        let height = 8u32;
        let rgb = vec![0u8; (width * height * 3) as usize];
        let out = rgb_to_i420(&rgb, width, height);
        assert_eq!(out.len(), (width * height * 3 / 2) as usize);
    }

    #[test]
    fn public_rgb_to_i420_matches_new_black() {
        // rgb_to_i420 of a black image should produce the same Y plane as
        // VideoFrame::new_black, which uses Y=16.
        let width = 8u32;
        let height = 8u32;
        let rgb = vec![0u8; (width * height * 3) as usize];
        let i420 = rgb_to_i420(&rgb, width, height);

        let y_size = (width * height) as usize;
        for &y in &i420[..y_size] {
            assert_eq!(y, 16, "Y plane mismatch for black");
        }
    }

    // ── Vp8Encoder as VideoSink ───────────────────────────────────────────────

    #[test]
    fn vp8_encoder_via_video_sink_trait() {
        let width = 64u32;
        let height = 64u32;
        let fps = 30u32;

        let (mut encoder, mut track) = Vp8Encoder::new(width, height, fps, 200, 16).unwrap();

        let frame = VideoFrame::new_black(width, height);

        // Call through the trait object to confirm the impl compiles and runs.
        let sink: &mut dyn VideoSink = &mut encoder;
        for _ in 0..5 {
            assert!(matches!(sink.push_frame(&frame).unwrap(), ControlFlow::Continue(())));
        }

        let mut received = 0;
        while track.try_recv().is_ok() {
            received += 1;
        }
        assert!(received > 0, "encoder must produce encoded frames");
    }

    #[test]
    fn vp8_encoder_sink_break_on_track_close() {
        let (mut encoder, track) = Vp8Encoder::new(64, 64, 30, 200, 16).unwrap();
        // Drop the receiver so the broadcast channel is closed.
        drop(track);

        let frame = VideoFrame::new_black(64, 64);
        let sink: &mut dyn VideoSink = &mut encoder;
        // Encoder should signal Break once it notices the receiver is gone.
        let mut got_break = false;
        for _ in 0..10 {
            match sink.push_frame(&frame).unwrap() {
                ControlFlow::Break(()) => {
                    got_break = true;
                    break;
                }
                ControlFlow::Continue(()) => {}
            }
        }
        assert!(got_break, "encoder should return Break when track receiver is dropped");
    }

    // ── encode → decode round-trip via VideoSink ──────────────────────────────

    #[test]
    fn encode_via_sink_decode_roundtrip() {
        use bytes::Bytes;
        use callme::codec::VP8_RTP_CLOCK_RATE;
        use iroh_roq::rtp::{
            codecs::vp8::{Vp8Packet, Vp8Payloader},
            packetizer::{new_packetizer, Packetizer},
            sequence::new_random_sequencer,
        };
        use webrtc_media::io::sample_builder::SampleBuilder;

        let width = 128u32;
        let height = 128u32;
        let fps = 30u32;

        let (mut encoder, mut track) = Vp8Encoder::new(width, height, fps, 500, 16).unwrap();
        let mut decoder = Vp8Decoder::new().unwrap();

        let frame = VideoFrame::new_black(width, height);
        let sink: &mut dyn VideoSink = &mut encoder;
        for _ in 0..3 {
            let _ = sink.push_frame(&frame).unwrap();
        }

        // Drain encoded payloads from the track.
        let mut payloads: Vec<Bytes> = Vec::new();
        while let Ok(mf) = track.try_recv() {
            payloads.push(mf.payload);
        }
        assert!(!payloads.is_empty(), "encoder must emit payloads");

        // RTP packetize → depacketize → VP8 decode.
        let rtp_dur = VP8_RTP_CLOCK_RATE / fps;
        let mut packetizer = new_packetizer(
            1100,
            98,
            0,
            Box::new(Vp8Payloader::default()),
            Box::new(new_random_sequencer()),
            VP8_RTP_CLOCK_RATE,
        );
        let mut sample_builder = SampleBuilder::new(10, Vp8Packet::default(), VP8_RTP_CLOCK_RATE);

        let mut decoded: Option<VideoFrame> = None;
        for payload in &payloads {
            for packet in packetizer.packetize(payload, rtp_dur).unwrap() {
                sample_builder.push(packet);
                while let Some(sample) = sample_builder.pop() {
                    if let Some(f) = decoder.decode(&sample.data).unwrap() {
                        decoded = Some(f);
                    }
                }
            }
        }

        let f = decoded.expect("must decode at least one frame");
        assert_eq!(f.width, width);
        assert_eq!(f.height, height);
        assert_eq!(f.data.len(), (width * height * 3 / 2) as usize);
    }

    // ── CameraConfig ──────────────────────────────────────────────────────────

    #[test]
    fn camera_config_default() {
        let cfg = CameraConfig::default();
        assert_eq!(cfg.index, 0);
        assert!(cfg.width > 0 && cfg.height > 0 && cfg.fps > 0);
    }

    /// Open the default camera and capture one frame.
    /// Skipped in CI because camera hardware is unavailable.
    #[test]
    #[ignore = "requires a physical camera device"]
    fn hardware_capture_one_frame() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let capture = callme::video::capture::VideoCapture::build(CameraConfig::default())
                .await
                .expect("failed to open camera");
            let mut track = capture.create_vp8_track(500).await.unwrap();

            // Wait up to 2 seconds for the first encoded frame.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            loop {
                if track.try_recv().is_ok() {
                    return;
                }
                if std::time::Instant::now() >= deadline {
                    panic!("no encoded frame received within 2 seconds");
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        });
    }
}
