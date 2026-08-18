// SPDX-License-Identifier: MPL-2.0

//! Print earshot's voice-activity score per frame for a raw f32 mono file.
//!
//! Used to calibrate the gate's threshold against real recordings.
//!
//! ```shell
//! cargo run --example vadscore -- audio.f32 16000
//! ```

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next().expect("usage: vadscore <raw-f32-mono> [rate]");
    let rate: usize = args
        .next()
        .and_then(|value| value.parse().ok())
        .unwrap_or(16_000);

    let bytes = std::fs::read(&path).expect("failed to read input");
    let samples: Vec<f32> = bytes
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect();

    let mut detector = earshot::Detector::default_boxed();
    for (index, frame) in samples.chunks_exact(256).enumerate() {
        let score = detector.predict_f32(frame);
        let peak = frame.iter().fold(0.0f32, |acc, s| acc.max(s.abs()));
        let ms = index * 256 * 1000 / rate;
        println!("{ms} {score:.4} {peak:.5}");
    }
}
