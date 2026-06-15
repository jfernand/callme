/// End-to-end codec round-trip: I420 frame → VP8 encode → RTP packetize →
/// RTP depacketize → VP8 decode → I420 frame.  No network involved.
use anyhow::Result;
use bytes::Bytes;
use iroh_roq::rtp::{
    codecs::vp8::{Vp8Packet, Vp8Payloader},
    packetizer::{new_packetizer, Packetizer},
    sequence::new_random_sequencer,
};
use webrtc_media::io::sample_builder::SampleBuilder;

use callme::{
    codec::VP8_RTP_CLOCK_RATE,
    video::{
        vp8::{Vp8Decoder, Vp8Encoder},
        VideoFrame,
    },
};

#[test]
fn vp8_encode_decode_roundtrip() -> Result<()> {
    let width = 128u32;
    let height = 128u32;
    let fps = 30u32;
    let rtp_duration = VP8_RTP_CLOCK_RATE / fps;

    let frame = VideoFrame::new_black(width, height);

    let (mut encoder, mut track) = Vp8Encoder::new(width, height, fps, 500, 16)?;
    let mut decoder = Vp8Decoder::new()?;

    // Push several frames so the encoder definitely emits at least one keyframe.
    for _ in 0..3 {
        let _ = encoder.push_frame(&frame)?;
    }

    // Collect encoded VP8 payloads from the broadcast track channel.
    let mut encoded_payloads: Vec<Bytes> = Vec::new();
    loop {
        match track.try_recv() {
            Ok(mf) => encoded_payloads.push(mf.payload),
            Err(_) => break,
        }
    }
    assert!(
        !encoded_payloads.is_empty(),
        "VP8 encoder produced no output frames"
    );

    // RTP round-trip ---------------------------------------------------
    let sequencer = Box::new(new_random_sequencer());
    let mut packetizer = new_packetizer(
        1100,
        98, // VP8 payload type
        0,  // SSRC
        Box::new(Vp8Payloader::default()),
        sequencer,
        VP8_RTP_CLOCK_RATE,
    );
    let mut sample_builder =
        SampleBuilder::new(10, Vp8Packet::default(), VP8_RTP_CLOCK_RATE);

    let mut decoded_frame: Option<VideoFrame> = None;
    for payload in &encoded_payloads {
        let packets = packetizer.packetize(payload, rtp_duration)?;
        for packet in packets {
            sample_builder.push(packet);
            while let Some(sample) = sample_builder.pop() {
                if let Some(f) = decoder.decode(&sample.data)? {
                    decoded_frame = Some(f);
                }
            }
        }
    }

    let decoded = decoded_frame.expect("should have decoded at least one VP8 frame");
    assert_eq!(decoded.width, width, "decoded width mismatch");
    assert_eq!(decoded.height, height, "decoded height mismatch");
    assert_eq!(
        decoded.data.len(),
        (width * height * 3 / 2) as usize,
        "decoded data length mismatch"
    );

    Ok(())
}
