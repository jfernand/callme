#[cfg(feature = "video")]
mod tests {
    use std::ops::ControlFlow;

    use callme::video::{
        capture::rgb_to_i420,
        render::{i420_to_rgba, MediaTrackVp8Decoder},
        vp8::Vp8Encoder,
        VideoFrame, VideoSource,
    };

    // ── i420_to_rgba public API ───────────────────────────────────────────────

    #[test]
    fn public_i420_to_rgba_size() {
        let frame = VideoFrame::new_black(16, 8);
        assert_eq!(i420_to_rgba(&frame).len(), 16 * 8 * 4);
    }

    #[test]
    fn public_i420_to_rgba_alpha_always_255() {
        let frame = VideoFrame::new_black(4, 4);
        let rgba = i420_to_rgba(&frame);
        for chunk in rgba.chunks_exact(4) {
            assert_eq!(chunk[3], 255, "alpha must be fully opaque");
        }
    }

    /// RGB → I420 → RGBA should approximately reproduce the original colour
    /// within the rounding error of BT.601 integer arithmetic.
    #[test]
    fn rgb_i420_rgba_roundtrip_colours() {
        let cases: &[(u8, u8, u8)] = &[
            (255, 0, 0),   // red
            (0, 255, 0),   // green
            (0, 0, 255),   // blue
            (255, 255, 0), // yellow
            (0, 0, 0),     // black
            (255, 255, 255), // white
        ];

        for &(r_in, g_in, b_in) in cases {
            let w = 4u32;
            let h = 4u32;
            let rgb: Vec<u8> = (0..w * h).flat_map(|_| [r_in, g_in, b_in]).collect();
            let i420 = rgb_to_i420(&rgb, w, h);
            let frame = VideoFrame { width: w, height: h, data: i420 };
            let rgba = i420_to_rgba(&frame);

            let r_out = rgba[0] as i32;
            let g_out = rgba[1] as i32;
            let b_out = rgba[2] as i32;
            let tol = 4i32; // ±4 for BT.601 rounding
            assert!(
                (r_out - r_in as i32).abs() <= tol,
                "R mismatch for ({r_in},{g_in},{b_in}): got {r_out}"
            );
            assert!(
                (g_out - g_in as i32).abs() <= tol,
                "G mismatch for ({r_in},{g_in},{b_in}): got {g_out}"
            );
            assert!(
                (b_out - b_in as i32).abs() <= tol,
                "B mismatch for ({r_in},{g_in},{b_in}): got {b_out}"
            );
        }
    }

    // ── MediaTrackVp8Decoder full pipeline ────────────────────────────────────

    #[test]
    fn full_encode_decode_rgba_pipeline() {
        let w = 128u32;
        let h = 128u32;
        let fps = 30u32;

        let (mut encoder, track) = Vp8Encoder::new(w, h, fps, 500, 32).unwrap();
        let mut decoder = MediaTrackVp8Decoder::new(track).unwrap();

        let frame = VideoFrame::new_black(w, h);
        for _ in 0..5 {
            encoder.push_frame(&frame).unwrap();
        }
        drop(encoder); // signal end-of-stream

        let mut decoded: Option<VideoFrame> = None;
        loop {
            match decoder.next_frame().unwrap() {
                ControlFlow::Continue(Some(f)) => decoded = Some(f),
                ControlFlow::Continue(None) => {}
                ControlFlow::Break(()) => break,
            }
        }

        let f = decoded.expect("must decode at least one frame");
        assert_eq!(f.width, w);
        assert_eq!(f.height, h);

        // Convert to RGBA and verify the image is mostly black.
        let rgba = i420_to_rgba(&f);
        assert_eq!(rgba.len(), (w * h * 4) as usize);
        let avg_luminance: f64 = rgba
            .chunks_exact(4)
            .map(|c| (c[0] as f64 + c[1] as f64 + c[2] as f64) / 3.0)
            .sum::<f64>()
            / (w * h) as f64;
        // VP8 is lossy, but a solid black frame should decode close to black.
        assert!(avg_luminance < 20.0, "expected near-black, got avg {avg_luminance:.1}");
    }

    #[test]
    fn video_source_trait_object() {
        // Confirm MediaTrackVp8Decoder works as a dyn VideoSource.
        let (_encoder, track) = Vp8Encoder::new(32, 32, 30, 200, 8).unwrap();
        // Keep _encoder alive so the broadcast channel stays open.
        let mut decoder: Box<dyn VideoSource> =
            Box::new(MediaTrackVp8Decoder::new(track).unwrap());
        // No frames encoded yet — channel open but empty → Continue(None).
        assert!(matches!(
            decoder.next_frame().unwrap(),
            ControlFlow::Continue(None)
        ));
    }
}
