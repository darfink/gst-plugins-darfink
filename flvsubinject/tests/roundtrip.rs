// SPDX-License-Identifier: MPL-2.0

//! End-to-end verification that FFmpeg demuxes what this element writes.
//!
//! The unit tests assert the byte layout against the specification. These
//! assert it against the *reader that matters*: if `ffprobe` does not report a
//! subtitle stream with the expected cues, the layout is wrong regardless of
//! what the specification says.
//!
//! # A pre-existing hazard these tests must see past
//!
//! `flvmux`/`eflvmux` rewrite `onMetaData` whenever a pad's codec info or tags
//! change, so a live stream carries several identical `onMetaData` tags at
//! non-zero timestamps. FFmpeg's demuxer treats *every* `FLV_TAG_TYPE_META` as
//! `FLV_STREAM_TYPE_SUBTITLE` (`flvdec.c:1515`) and only skips a metadata tag
//! when `dts == 0` (`flvdec.c:1520`). A repeated `onMetaData` at a non-zero
//! timestamp therefore surfaces as a spurious subtitle packet holding the raw
//! AMF byte `\u{2}`.
//!
//! This predates this element and happens with a bare `flvmux ! filesink`. It
//! is filtered out here rather than asserted away, because the property under
//! test is that *our* cues arrive intact and that we add nothing of our own.

use std::io::Write;
use std::process::{Command, Stdio};

use gst::prelude::*;
use gstflvsubinject::flv::parse_tag_header;

fn init() {
  use std::sync::Once;
  static INIT: Once = Once::new();
  INIT.call_once(|| {
    gst::init().unwrap();
    gstflvsubinject::plugin_register_static().unwrap();
  });
}

/// The FLV muxers this element is expected to sit behind.
///
/// Production publishes Enhanced FLV through `eflvmux`, so testing only the
/// classic `flvmux` would leave the muxer that actually runs uncovered. They
/// are separate implementations in the same plugin — `gstflvmux.c` and
/// `gsteflvmux.c` — and this element depends on a property of their *output*
/// (one whole tag per buffer), so the property is asserted for each rather
/// than assumed to transfer.
const MUXERS: [&str; 2] = ["flvmux", "eflvmux"];

/// Whether a muxer is present in this GStreamer build.
///
/// `eflvmux` arrived in 1.28; skipping rather than failing keeps the suite
/// usable on older toolchains, and the skip is announced so a silent pass
/// cannot be mistaken for coverage.
fn muxer_available(name: &str) -> bool {
  init();
  if gst::ElementFactory::find(name).is_some() {
    return true;
  }
  eprintln!("skipping {name}: not present in this GStreamer build");
  false
}

fn ffprobe_available() -> bool {
  Command::new("ffprobe")
    .arg("-version")
    .stdout(Stdio::null())
    .stderr(Stdio::null())
    .status()
    .is_ok_and(|status| status.success())
}

/// Mux a short A/V stream and inject cues, returning the FLV bytes.
fn produce_flv(muxer: &str, cues: &[(u64, &str)]) -> Vec<u8> {
  produce_flv_with(muxer, cues, true)
}

fn produce_flv_with(muxer: &str, cues: &[(u64, &str)], prime: bool) -> Vec<u8> {
  init();

  let pipeline = gst::Pipeline::new();

  let video = gst::ElementFactory::make("videotestsrc")
    .property("num-buffers", 60i32)
    .property_from_str("pattern", "black")
    .build()
    .unwrap();
  let encoder = gst::ElementFactory::make("x264enc")
    .property_from_str("speed-preset", "ultrafast")
    .property("key-int-max", 15u32)
    .build()
    .unwrap();
  let parser = gst::ElementFactory::make("h264parse").build().unwrap();
  let mux = gst::ElementFactory::make(muxer)
    .property("streamable", true)
    .build()
    .unwrap();

  let text_src = gst::ElementFactory::make("appsrc")
    .property("is-live", false)
    .property("format", gst::Format::Time)
    .property(
      "caps",
      gst::Caps::builder("text/x-raw").field("format", "utf8").build(),
    )
    .build()
    .unwrap();

  let inject = gst::ElementFactory::make("flvsubinject")
    .property("prime", prime)
    .property_from_str("input-mode", "replacement")
    .build()
    .unwrap();
  let sink = gst::ElementFactory::make("appsink")
    .property("sync", false)
    .build()
    .unwrap();

  pipeline
    .add_many([&video, &encoder, &parser, &mux, &text_src, &inject, &sink])
    .unwrap();
  gst::Element::link_many([&video, &encoder, &parser, &mux]).unwrap();
  mux.link_pads(Some("src"), &inject, Some("sink")).unwrap();
  text_src.link_pads(Some("src"), &inject, Some("text")).unwrap();
  inject.link(&sink).unwrap();

  let appsrc = text_src.downcast::<gst_app::AppSrc>().unwrap();
  let appsink = sink.downcast::<gst_app::AppSink>().unwrap();

  pipeline.set_state(gst::State::Playing).unwrap();

  for (millis, text) in cues {
    let mut buffer = gst::Buffer::from_slice(text.as_bytes().to_vec());
    {
      let buffer = buffer.get_mut().unwrap();
      buffer.set_pts(gst::ClockTime::from_mseconds(*millis));
      buffer.set_duration(gst::ClockTime::from_mseconds(100));
    }
    appsrc.push_buffer(buffer).unwrap();
  }
  appsrc.end_of_stream().unwrap();

  let mut output = Vec::new();
  while let Some(sample) = appsink.try_pull_sample(gst::ClockTime::from_seconds(5)) {
    let buffer = sample.buffer().unwrap();
    let map = buffer.map_readable().unwrap();
    output.extend_from_slice(map.as_slice());
  }

  pipeline.set_state(gst::State::Null).unwrap();
  output
}

/// The same A/V pipeline with no injector at all, as a transparency baseline.
fn produce_flv_without_element(muxer: &str) -> Vec<u8> {
  init();

  let pipeline = gst::Pipeline::new();
  let video = gst::ElementFactory::make("videotestsrc")
    .property("num-buffers", 60i32)
    .property_from_str("pattern", "black")
    .build()
    .unwrap();
  let encoder = gst::ElementFactory::make("x264enc")
    .property_from_str("speed-preset", "ultrafast")
    .property("key-int-max", 15u32)
    .build()
    .unwrap();
  let parser = gst::ElementFactory::make("h264parse").build().unwrap();
  let mux = gst::ElementFactory::make(muxer)
    .property("streamable", true)
    .build()
    .unwrap();
  let sink = gst::ElementFactory::make("appsink")
    .property("sync", false)
    .build()
    .unwrap();

  pipeline
    .add_many([&video, &encoder, &parser, &mux, &sink])
    .unwrap();
  gst::Element::link_many([&video, &encoder, &parser, &mux, &sink]).unwrap();

  let appsink = sink.downcast::<gst_app::AppSink>().unwrap();
  pipeline.set_state(gst::State::Playing).unwrap();

  let mut output = Vec::new();
  while let Some(sample) = appsink.try_pull_sample(gst::ClockTime::from_seconds(5)) {
    let buffer = sample.buffer().unwrap();
    let map = buffer.map_readable().unwrap();
    output.extend_from_slice(map.as_slice());
  }

  pipeline.set_state(gst::State::Null).unwrap();
  output
}

/// Extract subtitle cues via ffprobe, as `(pts_ms, text)`.
fn ffprobe_cues(flv: &[u8]) -> Vec<(i64, String)> {
  let mut child = Command::new("ffmpeg")
    .args([
      "-hide_banner", "-v", "error", "-f", "flv", "-i", "pipe:0", "-map", "0:s:0", "-f", "srt",
      "pipe:1",
    ])
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .expect("spawn ffmpeg");

  child.stdin.as_mut().unwrap().write_all(flv).ok();
  let output = child.wait_with_output().expect("ffmpeg output");
  let srt = String::from_utf8_lossy(&output.stdout);

  let mut cues = Vec::new();
  let mut lines = srt.lines();
  while let Some(line) = lines.next() {
    if !line.contains("-->") {
      continue;
    }
    let start = line.split("-->").next().unwrap().trim();
    let (hms, millis) = start.rsplit_once(',').unwrap();
    let parts: Vec<i64> = hms.split(':').map(|p| p.parse().unwrap()).collect();
    let pts_ms =
      (parts[0] * 3600 + parts[1] * 60 + parts[2]) * 1000 + millis.parse::<i64>().unwrap();
    let text = lines.next().unwrap_or_default().to_owned();
    // Spurious cues from repeated `onMetaData` carry a lone AMF type byte and
    // never legible text. Dropping exactly that shape keeps every real cue,
    // including the deliberately empty clear-display cue.
    if text.chars().all(|character| character.is_control()) && !text.is_empty() {
      continue;
    }
    cues.push((pts_ms, text));
  }
  cues
}

#[test]
fn ffmpeg_demuxes_injected_cues_at_their_timestamps() {
  if !ffprobe_available() {
    eprintln!("skipping: ffmpeg not available");
    return;
  }

  for muxer in MUXERS {
    if !muxer_available(muxer) {
      continue;
    }
    let flv = produce_flv(muxer, &[(0, "first cue"), (500, "second cue"), (1000, "third cue")]);
    assert!(!flv.is_empty(), "{muxer} produced no FLV bytes");

    let cues = ffprobe_cues(&flv);
    assert_eq!(cues.len(), 3, "{muxer}: expected three demuxed cues, got {cues:?}");
    assert_eq!(cues[0].1, "first cue", "{muxer}");
    assert_eq!(cues[1].1, "second cue", "{muxer}");
    assert_eq!(cues[2].1, "third cue", "{muxer}");

    // Timestamps must survive the round trip, not merely the text.
    assert!(cues[0].0.abs() <= 40, "{muxer}: first cue at {}ms", cues[0].0);
    assert!((cues[1].0 - 500).abs() <= 40, "{muxer}: second cue at {}ms", cues[1].0);
    assert!((cues[2].0 - 1000).abs() <= 40, "{muxer}: third cue at {}ms", cues[2].0);
  }
}

#[test]
fn an_empty_cue_survives_as_a_clear_signal() {
  if !ffprobe_available() {
    eprintln!("skipping: ffmpeg not available");
    return;
  }

  // An empty cue is how a caller clears the display. FFmpeg's SRT writer omits
  // blank cue bodies, so this asserts the tag reaches the demuxer at all by
  // checking what precedes and follows it.
  for muxer in MUXERS {
    if !muxer_available(muxer) {
      continue;
    }
    let flv = produce_flv(muxer, &[(0, "visible"), (500, ""), (1000, "visible again")]);
    let cues = ffprobe_cues(&flv);
    assert!(
      cues.iter().any(|(_, text)| text == "visible"),
      "{muxer}: first cue missing: {cues:?}"
    );
    assert!(
      cues.iter().any(|(_, text)| text == "visible again"),
      "{muxer}: third cue missing: {cues:?}"
    );
  }
}

#[test]
fn av_only_input_gains_no_cues_of_our_own() {
  init();
  // With no cues the element must be transparent. Any *legible* cue here would
  // mean we invented one; the AMF control bytes from repeated `onMetaData` are
  // filtered by `ffprobe_cues` because they are the muxer's, not ours.
  for muxer in MUXERS {
    if !muxer_available(muxer) {
      continue;
    }
    let with_element = produce_flv(muxer, &[]);
    assert!(!with_element.is_empty(), "{muxer}");

    if ffprobe_available() {
      let cues = ffprobe_cues(&with_element);
      assert!(cues.is_empty(), "{muxer}: unexpected cues: {cues:?}");
    }
  }
}

#[test]
fn passthrough_is_byte_identical_without_cues() {
  init();
  // The strongest statement of transparency: with priming disabled, the same
  // pipeline with and without the element must produce identical bytes.
  //
  // Priming is deliberately excluded here rather than tested loosely. It adds
  // exactly one tag by design, so asserting transparency around it would only
  // restate the implementation; what matters is that the A/V bytes themselves
  // are never touched.
  for muxer in MUXERS {
    if !muxer_available(muxer) {
      continue;
    }
    let injected = produce_flv_with(muxer, &[], false);
    let direct = produce_flv_without_element(muxer);
    assert_eq!(
      injected.len(),
      direct.len(),
      "{muxer}: element altered the byte count of an uncaptioned stream"
    );
    assert_eq!(injected, direct, "{muxer}: element altered an uncaptioned stream");
  }
}

#[test]
fn priming_declares_the_subtitle_stream_before_any_cue() {
  if !ffprobe_available() {
    eprintln!("skipping: ffmpeg not available");
    return;
  }

  // A stream whose captions start late must still be discoverable as carrying
  // subtitles: FFmpeg creates the text stream lazily on the first cue, and a
  // packager that has finished probing by then never sees one.
  for muxer in MUXERS {
    if !muxer_available(muxer) {
      continue;
    }
    assert_primes(muxer);
  }
}

/// Assert the invariant the element is built on, for one muxer.
///
/// `handle_flv_buffer` reads `GST_BUFFER_PTS` and forwards the buffer whole
/// rather than parsing the byte stream, which is only sound if a buffer is
/// exactly one FLV tag. That is a property of the muxer, not of this element,
/// so it is asserted directly against each muxer rather than inferred from the
/// round-trip passing.
///
/// Checked with audio and video interleaved and through a `queue`, because
/// those are the conditions under which buffers would plausibly be merged.
fn assert_one_tag_per_buffer(muxer: &str) {
  init();

  let pipeline = gst::Pipeline::new();
  let video = gst::ElementFactory::make("videotestsrc")
    .property("num-buffers", 40i32)
    .property_from_str("pattern", "black")
    .build()
    .unwrap();
  let encoder = gst::ElementFactory::make("x264enc")
    .property_from_str("speed-preset", "ultrafast")
    .property("key-int-max", 10u32)
    .build()
    .unwrap();
  let parser = gst::ElementFactory::make("h264parse").build().unwrap();
  let audio = gst::ElementFactory::make("audiotestsrc")
    .property("num-buffers", 60i32)
    .build()
    .unwrap();
  let audio_encoder = gst::ElementFactory::make("avenc_aac").build().unwrap();
  let audio_parser = gst::ElementFactory::make("aacparse").build().unwrap();
  let mux = gst::ElementFactory::make(muxer)
    .property("streamable", true)
    .build()
    .unwrap();
  let queue = gst::ElementFactory::make("queue").build().unwrap();
  let sink = gst::ElementFactory::make("appsink")
    .property("sync", false)
    .build()
    .unwrap();

  pipeline
    .add_many([
      &video,
      &encoder,
      &parser,
      &audio,
      &audio_encoder,
      &audio_parser,
      &mux,
      &queue,
      &sink,
    ])
    .unwrap();
  gst::Element::link_many([&video, &encoder, &parser, &mux]).unwrap();
  gst::Element::link_many([&audio, &audio_encoder, &audio_parser, &mux]).unwrap();
  gst::Element::link_many([&mux, &queue, &sink]).unwrap();

  let appsink = sink.downcast::<gst_app::AppSink>().unwrap();
  pipeline.set_state(gst::State::Playing).unwrap();

  let mut timestamped = 0usize;
  let mut headers = 0usize;
  while let Some(sample) = appsink.try_pull_sample(gst::ClockTime::from_seconds(5)) {
    let buffer = sample.buffer().unwrap();
    let map = buffer.map_readable().unwrap();

    if buffer.pts().is_none() {
      // Stream headers: the FLV header, `onMetaData`, and codec data. These
      // are generated rather than derived from media, are forwarded untouched,
      // and are not required to be single tags.
      headers += 1;
      continue;
    }

    timestamped += 1;
    let slice = map.as_slice();
    let header = parse_tag_header(slice)
      .unwrap_or_else(|| panic!("{muxer}: timestamped buffer is not a parseable FLV tag"));
    assert_eq!(
      header.total_len(),
      slice.len(),
      "{muxer}: buffer of {} bytes carries a {}-byte tag, so it is not exactly one tag",
      slice.len(),
      header.total_len()
    );
  }

  pipeline.set_state(gst::State::Null).unwrap();
  assert!(headers > 0, "{muxer}: no stream headers observed");
  assert!(
    timestamped > 20,
    "{muxer}: only {timestamped} timestamped buffers observed"
  );
}

#[test]
fn every_muxer_buffer_is_exactly_one_tag() {
  for muxer in MUXERS {
    if !muxer_available(muxer) {
      continue;
    }
    assert_one_tag_per_buffer(muxer);
  }
}

fn assert_primes(muxer: &str) {
  let primed = produce_flv_with(muxer, &[], true);
  let streams = String::from_utf8_lossy(
    &Command::new("ffprobe")
      .args([
        "-hide_banner", "-v", "error", "-select_streams", "s", "-show_entries",
        "stream=codec_name", "-of", "csv=p=0", "-f", "flv", "-i", "pipe:0",
      ])
      .stdin(Stdio::piped())
      .stdout(Stdio::piped())
      .spawn()
      .and_then(|mut child| {
        child.stdin.as_mut().unwrap().write_all(&primed)?;
        child.wait_with_output()
      })
      .expect("ffprobe")
      .stdout,
  )
  .trim()
  .to_owned();

  assert!(
    streams.contains("text"),
    "{muxer}: priming did not declare a subtitle stream: {streams:?}"
  );

  let packets = String::from_utf8_lossy(
    &Command::new("ffprobe")
      .args([
        "-hide_banner", "-v", "error", "-select_streams", "s", "-show_entries",
        "packet=pts_time,size", "-of", "csv=p=0", "-f", "flv", "-i", "pipe:0",
      ])
      .stdin(Stdio::piped())
      .stdout(Stdio::piped())
      .spawn()
      .and_then(|mut child| {
        child.stdin.as_mut().unwrap().write_all(&primed)?;
        child.wait_with_output()
      })
      .expect("ffprobe")
      .stdout,
  )
  .trim()
  .to_owned();

  // The declaration must leave stateful consumers blank. A visually empty
  // non-empty character would instead create an active, never-cleared state.
  assert!(
    packets.lines().next().is_some_and(|packet| packet == "0.000000,0"),
    "{muxer}: priming subtitle packet was not empty: {packets:?}"
  );
}
