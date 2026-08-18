// SPDX-License-Identifier: MPL-2.0

//! Two threads, one output stream.
//!
//! `flvsubinject` is a plain element with two independently-scheduled chain
//! functions, not an aggregator. An aggregator serializes its pads by
//! construction: `aggregate()` runs on one thread and pulls from every pad,
//! so nothing it emits can be misordered. This element instead has the FLV
//! streaming thread and the text streaming thread both reaching `src.push()`.
//!
//! These tests exist to pin down what that costs, because the answer is not
//! "nothing" and the failure is timing-dependent enough to hide in a passing
//! end-to-end run.

use std::sync::{Arc, Mutex};

use gst::prelude::*;
use gstflvsubinject::flv::{parse_tag_header, TAG_TYPE_SCRIPT_DATA};

fn init() {
  use std::sync::Once;
  static INIT: Once = Once::new();
  INIT.call_once(|| {
    gst::init().unwrap();
    gstflvsubinject::plugin_register_static().unwrap();
  });
}

/// Both FLV muxers this element is expected to sit behind.
///
/// Production publishes Enhanced FLV through `eflvmux`; covering only the
/// classic `flvmux` would leave the concurrency behaviour of the muxer that
/// actually runs untested.
const MUXERS: [&str; 2] = ["flvmux", "eflvmux"];

fn muxer_available(name: &str) -> bool {
  init();
  if gst::ElementFactory::find(name).is_some() {
    return true;
  }
  eprintln!("skipping {name}: not present in this GStreamer build");
  false
}

/// Every tag timestamp in an FLV byte stream, in the order written.
fn tag_timestamps(flv: &[u8]) -> Vec<(u8, u32)> {
  let mut cursor = 9 + 4;
  let mut tags = Vec::new();
  while let Some(header) = parse_tag_header(&flv[cursor.min(flv.len())..]) {
    if flv.len() - cursor < header.total_len() {
      break;
    }
    tags.push((header.tag_type, header.timestamp_ms));
    cursor += header.total_len();
  }
  tags
}

/// Push A/V through the injector while cues arrive from a separate thread.
///
/// The text appsrc is driven from its own thread with `is-live=false`, which
/// is how the transcode pipeline feeds it: an AppBridge consumer runs its own
/// streaming thread, independent of the one carrying muxed FLV.
fn run_concurrent(muxer: &str, cue_count: u64) -> Vec<u8> {
  init();

  let pipeline = gst::Pipeline::new();
  let video = gst::ElementFactory::make("videotestsrc")
    .property("num-buffers", 120i32)
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
  // A queue gives the text branch its own streaming thread, which is what
  // makes the two chain functions genuinely concurrent rather than serialized
  // by the pushing thread.
  let text_queue = gst::ElementFactory::make("queue").build().unwrap();
  let inject = gst::ElementFactory::make("flvsubinject")
    .property("prime", false)
    .property_from_str("input-mode", "replacement")
    .build()
    .unwrap();
  let sink = gst::ElementFactory::make("appsink")
    .property("sync", false)
    .build()
    .unwrap();

  pipeline
    .add_many([
      &video,
      &encoder,
      &parser,
      &mux,
      &text_src,
      &text_queue,
      &inject,
      &sink,
    ])
    .unwrap();
  gst::Element::link_many([&video, &encoder, &parser, &mux]).unwrap();
  mux.link_pads(Some("src"), &inject, Some("sink")).unwrap();
  text_src.link(&text_queue).unwrap();
  text_queue
    .link_pads(Some("src"), &inject, Some("text"))
    .unwrap();
  inject.link(&sink).unwrap();

  let appsrc = text_src.downcast::<gst_app::AppSrc>().unwrap();
  let appsink = sink.downcast::<gst_app::AppSink>().unwrap();
  let collected = Arc::new(Mutex::new(Vec::new()));

  pipeline.set_state(gst::State::Playing).unwrap();

  let pusher = std::thread::spawn(move || {
    for index in 0..cue_count {
      let text = format!("cue {index}");
      let mut buffer = gst::Buffer::from_slice(text.into_bytes());
      {
        let buffer = buffer.get_mut().unwrap();
        buffer.set_pts(gst::ClockTime::from_mseconds(index * 30));
        buffer.set_duration(gst::ClockTime::from_mseconds(20));
      }
      if appsrc.push_buffer(buffer).is_err() {
        break;
      }
    }
    let _ = appsrc.end_of_stream();
  });

  while let Some(sample) = appsink.try_pull_sample(gst::ClockTime::from_seconds(5)) {
    let buffer = sample.buffer().unwrap();
    let map = buffer.map_readable().unwrap();
    collected.lock().unwrap().extend_from_slice(map.as_slice());
  }

  pusher.join().unwrap();
  pipeline.set_state(gst::State::Null).unwrap();
  collected.lock().unwrap().clone()
}

#[test]
fn tag_framing_survives_concurrent_text_and_flv_threads() {
  // The load-bearing property. A script-data tag spliced into the middle of
  // another tag's body does not corrupt one caption: it desynchronizes the
  // tag stream, and every byte after it is garbage. A consumer sees EBML-style
  // nonsense rather than a missing cue.
  for muxer in MUXERS {
    if !muxer_available(muxer) {
      continue;
    }
    let flv = run_concurrent(muxer, 60);
    assert!(!flv.is_empty(), "{muxer}: no output produced");

    let tags = tag_timestamps(&flv);
    assert!(tags.len() > 10, "{muxer}: only {} tags parsed", tags.len());

    // Walking the whole stream by header length and landing exactly on the end
    // is the check: it can only succeed if every tag boundary is intact.
    let mut cursor = 9 + 4;
    for (index, _) in tags.iter().enumerate() {
      let header = parse_tag_header(&flv[cursor..])
        .unwrap_or_else(|| panic!("{muxer}: tag {index} has an unparseable header"));
      cursor += header.total_len();
    }
    assert_eq!(
      cursor,
      flv.len(),
      "{muxer}: tag stream does not end on a boundary: framing was corrupted"
    );
  }
}

#[test]
fn script_data_timestamps_never_regress() {
  // Ordering, as distinct from framing. A consumer that trusts tag order will
  // mis-time captions if a cue is written after a later-timestamped A/V tag.
  for muxer in MUXERS {
    if !muxer_available(muxer) {
      continue;
    }
    assert_no_regressions(muxer);
  }
}

fn assert_no_regressions(muxer: &str) {
  let flv = run_concurrent(muxer, 60);
  let tags = tag_timestamps(&flv);

  let mut highest = 0u32;
  let mut regressions = Vec::new();
  for (tag_type, timestamp) in &tags {
    if *timestamp < highest {
      regressions.push((*tag_type, *timestamp, highest));
    }
    highest = highest.max(*timestamp);
  }

  assert!(
    regressions.is_empty(),
    "{muxer}: {} tags went backwards in time, first: {:?}",
    regressions.len(),
    regressions.first()
  );

  let script_tags = tags
    .iter()
    .filter(|(tag_type, _)| *tag_type == TAG_TYPE_SCRIPT_DATA)
    .count();
  assert!(script_tags > 0, "{muxer}: no cues were written at all");
}

#[test]
fn every_cue_is_written_between_two_whole_tags() {
  // The framing property stated positively, and the one that would have caught
  // a text-thread push directly: every script-data tag must begin exactly where
  // the preceding tag ended.
  //
  // Walking by header length already proves this globally, but doing it while
  // asserting that script tags appear at boundaries makes the intent explicit,
  // so a future change that reintroduces a second writer fails here with a
  // readable message rather than as "stream ends mid-tag".
  for muxer in MUXERS {
    if !muxer_available(muxer) {
      continue;
    }
    assert_cues_land_on_boundaries(muxer);
  }
}

fn assert_cues_land_on_boundaries(muxer: &str) {
  let flv = run_concurrent(muxer, 80);
  let mut cursor = 9 + 4;
  let mut script_tags = 0;

  while cursor < flv.len() {
    let Some(header) = parse_tag_header(&flv[cursor..]) else {
      panic!("{muxer}: unparseable tag header at byte {cursor} of {}", flv.len());
    };
    assert!(
      flv.len() - cursor >= header.total_len(),
      "{muxer}: tag at byte {cursor} claims {} bytes but only {} remain",
      header.total_len(),
      flv.len() - cursor
    );
    if header.tag_type == TAG_TYPE_SCRIPT_DATA {
      script_tags += 1;
    }
    cursor += header.total_len();
  }

  assert_eq!(cursor, flv.len(), "{muxer}: stream does not end on a tag boundary");
  assert!(script_tags > 0, "{muxer}: no cues were written at all");
}
