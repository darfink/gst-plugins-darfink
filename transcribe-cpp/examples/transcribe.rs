// SPDX-License-Identifier: MPL-2.0

//! Run the transcriber over a file or a live RTP stream and print what it
//! produces.
//!
//! `gst-launch-1.0` cannot connect to signals, so this is the only way to
//! watch the volatile hypothesis (`partial-transcript`) alongside the
//! committed text that reaches the pad.
//!
//! ```shell
//! cargo run --example transcribe -- model.gguf speech.wav
//! cargo run --example transcribe -- model.gguf udp://5004
//! cargo run --example transcribe -- model.gguf speech.wav mode=chunked chunk-duration=8000
//! ```
//!
//! Committed buffers are printed with their timestamps; partials are printed
//! dimmed and overwritten in place, the way a live caption view would.

use gst::prelude::*;

const USAGE: &str = "usage: transcribe <model.gguf> <speech.wav|udp://PORT> [property=value ...]";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let model = args.next().ok_or(USAGE)?;
    let input = args.next().ok_or(USAGE)?;
    let properties: Vec<String> = args.collect();

    gst::init()?;
    gsttranscribecpp::plugin_register_static()?;

    let pipeline = gst::Pipeline::new();
    let transcriber = gst::ElementFactory::make("transcribecpptranscriber")
        .property("model-path", &model)
        .build()?;

    for property in &properties {
        let (name, value) = property
            .split_once('=')
            .ok_or_else(|| format!("expected property=value, got {property}"))?;
        transcriber.set_property_from_str(name, value);
    }

    let source = if let Some(port) = input.strip_prefix("udp://") {
        live_source(port.parse()?)?
    } else {
        file_source(&input)?
    };

    let convert = gst::ElementFactory::make("audioconvert").build()?;
    let resample = gst::ElementFactory::make("audioresample").build()?;
    let sink = gst::ElementFactory::make("fakesink").build()?;

    pipeline.add_many([&source, &convert, &resample, &transcriber, &sink])?;
    gst::Element::link_many([&source, &convert, &resample, &transcriber, &sink])?;

    transcriber.connect("partial-transcript", false, |args| {
        let text: String = args[1].get().unwrap();
        // Carriage return, no newline: the next partial overwrites this one.
        print!("\r\x1b[2m… {text}\x1b[0m\x1b[K");
        use std::io::Write;
        let _ = std::io::stdout().flush();
        None
    });

    transcriber
        .static_pad("src")
        .expect("transcriber has a src pad")
        .add_probe(gst::PadProbeType::BUFFER, |_, info| {
            if let Some(buffer) = info.buffer()
                && let Ok(map) = buffer.map_readable()
            {
                let text = String::from_utf8_lossy(&map);
                println!(
                    "\r\x1b[K[{:>12} + {:>10}] {text}",
                    buffer.pts().display(),
                    buffer.duration().display(),
                );
            }
            gst::PadProbeReturn::Ok
        });

    pipeline.set_state(gst::State::Playing)?;
    let result = run(&pipeline);
    pipeline.set_state(gst::State::Null)?;
    result
}

/// A plain file source. `decodebin` would work too; wavparse keeps the graph
/// small and the failure modes obvious.
fn file_source(location: &str) -> Result<gst::Element, gst::glib::BoolError> {
    let bin = gst::Bin::new();
    let src = gst::ElementFactory::make("filesrc")
        .property("location", location)
        .build()?;
    let parse = gst::ElementFactory::make("wavparse").build()?;

    bin.add_many([&src, &parse])?;
    gst::Element::link_many([&src, &parse])?;

    let pad = parse.static_pad("src").expect("wavparse has a src pad");
    bin.add_pad(&gst::GhostPad::with_target(&pad)?)?;

    Ok(bin.upcast())
}

/// RTP L16 over UDP — a genuinely live source, which is what makes the element
/// take its live path. `clocksync` on a file does not: the element keys off the
/// latency query, and only a live source answers it as live.
///
/// Feed it with:
///
/// ```shell
/// gst-launch-1.0 filesrc location=speech.wav ! wavparse ! audioconvert ! \
///   audioresample ! audio/x-raw,rate=16000,channels=1 ! audioconvert ! \
///   rtpL16pay pt=96 ! udpsink host=127.0.0.1 port=5004 sync=true
/// ```
fn live_source(port: i32) -> Result<gst::Element, gst::glib::BoolError> {
    let bin = gst::Bin::new();
    let caps = gst::Caps::builder("application/x-rtp")
        .field("media", "audio")
        .field("clock-rate", 16_000i32)
        .field("encoding-name", "L16")
        .field("channels", 1i32)
        .field("payload", 96i32)
        .build();

    let src = gst::ElementFactory::make("udpsrc")
        .property("port", port)
        .property("caps", &caps)
        .build()?;
    let jitter = gst::ElementFactory::make("rtpjitterbuffer")
        .property("latency", 200u32)
        .build()?;
    let depay = gst::ElementFactory::make("rtpL16depay").build()?;

    bin.add_many([&src, &jitter, &depay])?;
    gst::Element::link_many([&src, &jitter, &depay])?;

    let pad = depay.static_pad("src").expect("depay has a src pad");
    bin.add_pad(&gst::GhostPad::with_target(&pad)?)?;

    Ok(bin.upcast())
}

fn run(pipeline: &gst::Pipeline) -> Result<(), Box<dyn std::error::Error>> {
    let bus = pipeline.bus().expect("pipeline has a bus");

    for message in bus.iter_timed(gst::ClockTime::NONE) {
        use gst::MessageView::*;
        match message.view() {
            Eos(_) => break,
            Error(err) => {
                return Err(format!(
                    "{}: {} ({})",
                    err.src().map(|s| s.path_string()).unwrap_or_default(),
                    err.error(),
                    err.debug().unwrap_or_default()
                )
                .into());
            }
            Progress(progress) => {
                let (type_, code, text) = progress.get();
                println!("[{type_:?}] {code}: {text}");
            }
            _ => (),
        }
    }

    Ok(())
}
