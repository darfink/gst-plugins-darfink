// SPDX-License-Identifier: MPL-2.0

//! Direct aggregator tests. More end-to-end cases can use the same fixture so
//! that a missing watermark never turns into an unbounded test hang.

use gst::prelude::*;
use gstflvsubmux::flv::{TAG_TYPE_SCRIPT_DATA, parse_tag_header};
use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Barrier};

fn init() {
  use std::sync::Once;
  static INIT: Once = Once::new();
  INIT.call_once(|| {
    gst::init().unwrap();
    gstflvsubmux::plugin_register_static().unwrap();
  });
}

struct Harness {
  flv: gst_check::Harness,
  text: gst_check::Harness,
  bus: gst::Bus,
}

impl Harness {
  fn new() -> Self {
    init();
    let mut flv = gst_check::Harness::with_padnames("flvsubmux", Some("sink"), Some("src"));
    let element = flv.element().unwrap();
    element.set_property("prime", false);
    // Attach a bus up front so protocol errors raised on the aggregate thread
    // are observable; a bare element otherwise has nowhere to post them.
    let bus = gst::Bus::new();
    element.set_bus(Some(&bus));
    let mut text = gst_check::Harness::with_element(&element, Some("text"), None);
    flv.set_src_caps_str("video/x-flv");
    text.set_src_caps_str("text/x-raw, format=utf8");
    flv.play();
    Self { flv, text, bus }
  }

  fn push_gap(&mut self, start_ms: u64, duration_ms: u64) {
    assert!(
      self.text.push_event(
        gst::event::Gap::builder(gst::ClockTime::from_mseconds(start_ms))
          .duration(gst::ClockTime::from_mseconds(duration_ms))
          .build()
      )
    );
  }

  /// Push a SEGMENT carrying `rate` on the given sink pad.
  fn push_segment_with_rate(&mut self, pad: &str, rate: f64) -> bool {
    let mut segment = gst::FormattedSegment::<gst::ClockTime>::new();
    segment.set_rate(rate);
    let event = gst::event::Segment::new(&segment);
    if pad == "sink" {
      self.flv.push_event(event)
    } else {
      self.text.push_event(event)
    }
  }

  /// Whether the element posted an ERROR within a bounded wait.
  fn posted_error(&self) -> bool {
    for _ in 0..20 {
      if let Some(message) = self.bus.timed_pop(gst::ClockTime::from_mseconds(50))
        && matches!(message.view(), gst::MessageView::Error(_))
      {
        return true;
      }
    }
    false
  }

  fn push_text(&mut self, start_ms: u64, duration_ms: u64, text: &str) {
    let mut buffer = gst::Buffer::from_slice(text.as_bytes().to_vec());
    {
      let buffer = buffer.get_mut().unwrap();
      buffer.set_pts(gst::ClockTime::from_mseconds(start_ms));
      buffer.set_duration(gst::ClockTime::from_mseconds(duration_ms));
    }
    self.text.push(buffer).unwrap();
  }

  fn make_flv(timestamp_ms: u64) -> gst::Buffer {
    let body = [0x17u8, 0x01, 0, 0, 0];
    let mut tag = vec![9u8];
    tag.extend_from_slice(&(body.len() as u32).to_be_bytes()[1..]);
    let timestamp = timestamp_ms as u32;
    tag.extend_from_slice(&timestamp.to_be_bytes()[1..]);
    tag.push((timestamp >> 24) as u8);
    tag.extend_from_slice(&[0, 0, 0]);
    tag.extend_from_slice(&body);
    tag.extend_from_slice(&((11 + body.len()) as u32).to_be_bytes());
    let mut buffer = gst::Buffer::from_mut_slice(tag);
    {
      let buffer = buffer.get_mut().unwrap();
      buffer.set_pts(gst::ClockTime::from_mseconds(timestamp_ms));
      buffer.set_duration(gst::ClockTime::from_mseconds(100));
    }
    buffer
  }

  fn push_flv(&mut self, timestamp_ms: u64) {
    self.flv.push(Self::make_flv(timestamp_ms)).unwrap();
  }

  fn output_headers(&mut self, count: usize) -> Vec<(u32, u8)> {
    let mut headers = Vec::new();
    for _ in 0..count {
      let buffer = self.flv.pull().unwrap();
      let map = buffer.map_readable().unwrap();
      let header = parse_tag_header(map.as_slice()).unwrap();
      headers.push((header.timestamp_ms, header.tag_type));
    }
    headers
  }

  fn drain_headers(&mut self) -> Vec<(u32, u8)> {
    let mut headers = Vec::new();
    while let Some(buffer) = self.flv.try_pull() {
      let map = buffer.map_readable().unwrap();
      let header = parse_tag_header(map.as_slice()).unwrap();
      headers.push((header.timestamp_ms, header.tag_type));
    }
    headers
  }

  /// Block the aggregate thread inside its downstream push.
  ///
  /// Returns a barrier that releases the push when waited on, standing in for
  /// a slow sink such as `rtmp2sink` applying backpressure.
  fn block_downstream(&mut self) -> (Arc<Barrier>, Arc<Barrier>) {
    let entered = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    let entered_probe = entered.clone();
    let release_probe = release.clone();
    self
      .flv
      .element()
      .unwrap()
      .static_pad("src")
      .unwrap()
      .add_probe(
        gst::PadProbeType::BUFFER | gst::PadProbeType::BUFFER_LIST,
        move |_pad, _info| {
          entered_probe.wait();
          release_probe.wait();
          gst::PadProbeReturn::Ok
        },
      )
      .unwrap();
    (entered, release)
  }
}

/// Run `action` on another thread and report whether it finished in time.
///
/// Used to assert that the event path stays responsive: a failure here means
/// the call blocked behind the media branch rather than completing on its own.
fn completes_within(timeout: std::time::Duration, action: impl FnOnce() + Send + 'static) -> bool {
  let (done_tx, done_rx) = mpsc::channel::<()>();
  std::thread::spawn(move || {
    action();
    let _ = done_tx.send(());
  });
  done_rx.recv_timeout(timeout).is_ok()
}

#[test]
fn one_gap_watermarks_multiple_flv_buffers() {
  let mut harness = Harness::new();
  harness.push_gap(0, 500);
  harness.push_flv(0);
  harness.push_flv(100);
  harness.push_flv(400);
  assert_eq!(harness.output_headers(3), vec![(0, 9), (100, 9), (400, 9)]);
}

#[test]
fn one_text_interval_covers_multiple_flv_buffers() {
  let mut harness = Harness::new();
  harness.push_text(0, 500, "word");
  harness.push_flv(0);
  harness.push_flv(100);
  harness.push_flv(400);
  assert_eq!(
    harness.output_headers(4),
    vec![(0, TAG_TYPE_SCRIPT_DATA), (0, 9), (100, 9), (400, 9)]
  );
}

#[test]
fn gap_is_not_decoded_as_utf8() {
  let mut harness = Harness::new();
  harness.push_gap(0, 200);
  harness.push_flv(0);
  harness.push_flv(100);
  assert_eq!(harness.output_headers(2), vec![(0, 9), (100, 9)]);
}

#[test]
fn media_waits_for_the_next_gap_after_the_watermark_ends() {
  let mut harness = Harness::new();
  harness.push_gap(0, 100);
  harness.push_flv(0);
  assert_eq!(harness.output_headers(1), vec![(0, 9)]);

  harness.push_flv(100);
  std::thread::sleep(std::time::Duration::from_millis(10));
  assert!(harness.flv.try_pull().is_none());

  harness.push_gap(100, 100);
  assert_eq!(harness.output_headers(1), vec![(100, 9)]);
}

#[test]
fn a_delayed_word_keeps_its_original_timestamp_after_gap_coverage() {
  let mut harness = Harness::new();
  harness.push_gap(0, 200);
  harness.push_text(200, 100, "delayed");
  harness.push_flv(0);
  harness.push_flv(100);
  harness.push_flv(200);
  assert_eq!(
    harness.output_headers(4),
    vec![(0, 9), (100, 9), (200, TAG_TYPE_SCRIPT_DATA), (200, 9)]
  );
}

#[test]
fn timed_clear_is_emitted_at_the_interval_boundary() {
  let mut harness = Harness::new();
  harness.push_text(0, 100, "timed");
  harness.text.push_event(gst::event::Eos::new());
  harness.push_flv(0);
  harness.push_flv(100);
  assert_eq!(
    harness.output_headers(4),
    vec![
      (0, TAG_TYPE_SCRIPT_DATA),
      (0, 9),
      (100, TAG_TYPE_SCRIPT_DATA),
      (100, 9)
    ]
  );
}

#[test]
fn replacement_duration_does_not_clear_state() {
  let mut harness = Harness::new();
  harness
    .flv
    .element()
    .unwrap()
    .set_property_from_str("input-mode", "replacement");
  harness.push_text(0, 100, "persistent");
  harness.text.push_event(gst::event::Eos::new());
  harness.push_flv(0);
  harness.push_flv(200);
  assert_eq!(
    harness.output_headers(3),
    vec![(0, TAG_TYPE_SCRIPT_DATA), (0, 9), (200, 9)]
  );
}

#[test]
fn same_timestamp_transitions_coalesce() {
  let mut harness = Harness::new();
  harness.push_text(0, 100, "first");
  harness.push_text(100, 100, "final");
  harness.push_flv(0);
  harness.push_flv(100);
  assert_eq!(
    harness.output_headers(4),
    vec![
      (0, TAG_TYPE_SCRIPT_DATA),
      (0, 9),
      (100, TAG_TYPE_SCRIPT_DATA),
      (100, 9)
    ]
  );
}

#[test]
fn text_eos_allows_remaining_media_to_drain() {
  let mut harness = Harness::new();
  harness.text.push_event(gst::event::Eos::new());
  harness.push_flv(0);
  harness.push_flv(100);
  assert_eq!(harness.output_headers(2), vec![(0, 9), (100, 9)]);
}

#[test]
fn pts_less_flv_header_passes_before_the_first_media_tag_and_prime_is_at_media_start() {
  let mut harness = Harness::new();
  harness.flv.element().unwrap().set_property("prime", true);
  harness.push_gap(0, 200);
  let header = gst::Buffer::from_slice(vec![b'F', b'L', b'V', 1, 5, 0, 0, 0, 9, 0, 0, 0, 0]);
  harness.flv.push(header.clone()).unwrap();
  let pulled_header = harness.flv.pull().unwrap();
  assert_eq!(
    pulled_header.map_readable().unwrap().as_slice(),
    header.map_readable().unwrap().as_slice()
  );
  harness.push_flv(0);
  assert_eq!(
    harness.output_headers(2),
    vec![(0, TAG_TYPE_SCRIPT_DATA), (0, 9)]
  );
}

#[test]
fn flush_stop_discards_the_old_interval_and_caption_state() {
  let mut harness = Harness::new();
  harness.push_text(0, 500, "stale");
  harness.push_flv(0);
  assert_eq!(harness.output_headers(2).len(), 2);
  harness.flv.push_event(gst::event::FlushStart::new());
  harness.text.push_event(gst::event::FlushStart::new());
  harness.flv.push_event(gst::event::FlushStop::new(true));
  harness.text.push_event(gst::event::FlushStop::new(true));
  harness
    .flv
    .push_event(gst::event::Segment::new(&gst::FormattedSegment::<
      gst::ClockTime,
    >::new()));
  harness
    .text
    .push_event(gst::event::Segment::new(&gst::FormattedSegment::<
      gst::ClockTime,
    >::new()));
  harness.push_gap(0, 200);
  harness.push_flv(0);
  assert_eq!(harness.output_headers(1), vec![(0, 9)]);
}

#[test]
fn final_text_beyond_media_is_discarded_only_after_text_eos() {
  let mut harness = Harness::new();
  harness.push_gap(0, 200);
  harness.push_text(200, 100, "future");
  harness.push_flv(0);
  assert_eq!(harness.output_headers(1), vec![(0, 9)]);
  harness.flv.push_event(gst::event::Eos::new());
  harness.text.push_event(gst::event::Eos::new());
  std::thread::sleep(std::time::Duration::from_millis(20));
  assert!(harness.flv.try_pull().is_none());
}

#[test]
fn a_word_inside_a_consumed_gap_fails_the_stream() {
  assert!(protocol_error_is_posted(true));
}

/// A downstream push must not block the text branch.
///
/// The element exists to decouple transcription from media flow. If the
/// aggregate thread holds shared state across `finish_buffer_list`, coverage
/// cannot be delivered while the sink is backpressured, which reintroduces
/// the coupling the aggregator design removes.
#[test]
fn text_coverage_is_accepted_while_media_is_backpressured() {
  let mut harness = Harness::new();
  let (entered, release) = harness.block_downstream();
  harness.push_gap(0, 500);
  harness.push_flv(0);

  entered.wait();

  let text_pad = harness.flv.element().unwrap().static_pad("text").unwrap();
  let delivered = completes_within(std::time::Duration::from_secs(2), move || {
    text_pad.send_event(
      gst::event::Gap::builder(gst::ClockTime::from_mseconds(500))
        .duration(gst::ClockTime::from_mseconds(500))
        .build(),
    );
  });

  release.wait();
  assert!(
    delivered,
    "text coverage could not be queued while the media branch was backpressured"
  );
}

/// A flush must be able to interrupt an in-flight downstream push.
///
/// `FLUSH_START` exists precisely to unblock a stalled pipeline, so it must
/// never wait on a lock held for the duration of that push.
#[test]
fn flush_start_is_not_blocked_by_an_in_flight_push() {
  let mut harness = Harness::new();
  let (entered, release) = harness.block_downstream();
  harness.push_gap(0, 500);
  harness.push_flv(0);

  entered.wait();

  let sink_pad = harness.flv.element().unwrap().static_pad("sink").unwrap();
  let flushed = completes_within(std::time::Duration::from_secs(3), move || {
    sink_pad.send_event(gst::event::FlushStart::new());
  });

  release.wait();
  assert!(
    flushed,
    "FLUSH_START could not reach the element during a downstream push"
  );
}

/// A segment on the text pad rebases text only.
///
/// `origin`, priming and the emitted tag timestamps are calibrated from the
/// media pad. Resetting them because the text branch started a new segment
/// would restart the subtitle timeline mid-stream, emitting a cue at
/// timestamp 0 after media had already advanced past it.
#[test]
fn a_text_segment_does_not_restart_the_media_timeline() {
  let mut harness = Harness::new();
  harness.flv.element().unwrap().set_property("prime", true);
  harness.push_gap(0, 100);
  harness.push_flv(0);
  assert_eq!(
    harness.output_headers(2),
    vec![(0, TAG_TYPE_SCRIPT_DATA), (0, 9)]
  );

  // Queued media that is not yet covered by the text timeline.
  harness.push_flv(100);
  std::thread::sleep(std::time::Duration::from_millis(50));
  assert!(harness.flv.try_pull().is_none(), "expected to block");

  harness
    .text
    .push_event(gst::event::Segment::new(&gst::FormattedSegment::<
      gst::ClockTime,
    >::new()));
  harness.push_gap(100, 100);

  let headers = harness.output_headers(1);
  assert_eq!(headers, vec![(100, 9)]);
  assert!(
    !harness
      .drain_headers()
      .iter()
      .any(|(timestamp, tag)| *tag == TAG_TYPE_SCRIPT_DATA && *timestamp == 0),
    "text segment re-primed the subtitle timeline at 0 after media reached 100"
  );
}

fn protocol_error_is_posted(late_word: bool) -> bool {
  init();
  let pipeline = gst::Pipeline::new();
  let flv_src = gst::ElementFactory::make("appsrc")
    .property("format", gst::Format::Time)
    .property("caps", gst::Caps::builder("video/x-flv").build())
    .build()
    .unwrap();
  let text_src = gst::ElementFactory::make("appsrc")
    .property("format", gst::Format::Time)
    .property(
      "caps",
      gst::Caps::builder("text/x-raw")
        .field("format", "utf8")
        .build(),
    )
    .build()
    .unwrap();
  let submux = gst::ElementFactory::make("flvsubmux")
    .property("prime", false)
    .build()
    .unwrap();
  let sink = gst::ElementFactory::make("fakesink").build().unwrap();
  pipeline
    .add_many([&flv_src, &text_src, &submux, &sink])
    .unwrap();
  flv_src
    .link_pads(Some("src"), &submux, Some("sink"))
    .unwrap();
  text_src
    .link_pads(Some("src"), &submux, Some("text"))
    .unwrap();
  submux.link(&sink).unwrap();
  let flv_src = flv_src.downcast::<gst_app::AppSrc>().unwrap();
  let text_src = text_src.downcast::<gst_app::AppSrc>().unwrap();
  let bus = pipeline.bus().unwrap();
  pipeline.set_state(gst::State::Playing).unwrap();

  if late_word {
    assert!(
      text_src.send_event(
        gst::event::Gap::builder(gst::ClockTime::ZERO)
          .duration(gst::ClockTime::from_mseconds(100))
          .build()
      )
    );
    flv_src.push_buffer(Harness::make_flv(0)).unwrap();
    flv_src.push_buffer(Harness::make_flv(100)).unwrap();
    let mut word = gst::Buffer::from_slice(b"late".to_vec());
    {
      let word = word.get_mut().unwrap();
      word.set_pts(gst::ClockTime::from_mseconds(50));
      word.set_duration(gst::ClockTime::from_mseconds(50));
    }
    text_src.push_buffer(word).unwrap();
  } else {
    flv_src.push_buffer(Harness::make_flv(0)).unwrap();
    assert!(
      text_src.send_event(
        gst::event::Gap::builder(gst::ClockTime::from_mseconds(100))
          .duration(gst::ClockTime::from_mseconds(100))
          .build()
      )
    );
  }

  let mut posted = false;
  while let Some(message) = bus.timed_pop(gst::ClockTime::from_mseconds(100)) {
    if matches!(message.view(), gst::MessageView::Error(_)) {
      posted = true;
      break;
    }
  }
  pipeline.set_state(gst::State::Null).unwrap();
  posted
}

#[test]
fn missing_initial_gap_fails_before_timestamped_media_is_emitted() {
  assert!(protocol_error_is_posted(false));
}

fn run_muxer(muxer_name: &str) -> Vec<u8> {
  init();
  let pipeline = gst::Pipeline::new();
  let video = gst::ElementFactory::make("videotestsrc")
    .property("num-buffers", 30i32)
    .property_from_str("pattern", "black")
    .build()
    .unwrap();
  let encoder = gst::ElementFactory::make("x264enc")
    .property_from_str("speed-preset", "ultrafast")
    .build()
    .unwrap();
  let parser = gst::ElementFactory::make("h264parse").build().unwrap();
  let mux = gst::ElementFactory::make(muxer_name)
    .property("streamable", true)
    .build()
    .unwrap();
  let text_src = gst::ElementFactory::make("appsrc")
    .property("format", gst::Format::Time)
    .property(
      "caps",
      gst::Caps::builder("text/x-raw")
        .field("format", "utf8")
        .build(),
    )
    .build()
    .unwrap();
  let submux = gst::ElementFactory::make("flvsubmux")
    .property("prime", false)
    .build()
    .unwrap();
  let sink = gst::ElementFactory::make("appsink")
    .property("sync", false)
    .build()
    .unwrap();

  pipeline
    .add_many([&video, &encoder, &parser, &mux, &text_src, &submux, &sink])
    .unwrap();
  gst::Element::link_many([&video, &encoder, &parser, &mux]).unwrap();
  mux.link_pads(Some("src"), &submux, Some("sink")).unwrap();
  text_src
    .link_pads(Some("src"), &submux, Some("text"))
    .unwrap();
  submux.link(&sink).unwrap();

  let text_src = text_src.downcast::<gst_app::AppSrc>().unwrap();
  let sink = sink.downcast::<gst_app::AppSink>().unwrap();
  pipeline.set_state(gst::State::Playing).unwrap();
  assert!(
    text_src.send_event(
      gst::event::Gap::builder(gst::ClockTime::ZERO)
        .duration(gst::ClockTime::from_mseconds(100))
        .build()
    )
  );
  let mut word = gst::Buffer::from_slice(b"hello".to_vec());
  {
    let word = word.get_mut().unwrap();
    word.set_pts(gst::ClockTime::from_mseconds(100));
    word.set_duration(gst::ClockTime::from_mseconds(100));
  }
  text_src.push_buffer(word).unwrap();
  assert!(
    text_src.send_event(
      gst::event::Gap::builder(gst::ClockTime::from_mseconds(200))
        .duration(gst::ClockTime::from_mseconds(1_000))
        .build()
    )
  );
  text_src.end_of_stream().unwrap();

  let mut output = Vec::new();
  while let Some(sample) = sink.try_pull_sample(gst::ClockTime::from_seconds(5)) {
    let buffer = sample.buffer().unwrap();
    output.extend_from_slice(buffer.map_readable().unwrap().as_slice());
  }
  pipeline.set_state(gst::State::Null).unwrap();
  output
}

#[test]
fn both_flv_muxers_feed_the_strict_aggregator() {
  init();
  for muxer in ["flvmux", "eflvmux"] {
    if gst::ElementFactory::find(muxer).is_none() {
      eprintln!("skipping {muxer}: not present in this GStreamer build");
      continue;
    }
    let output = run_muxer(muxer);
    assert!(!output.is_empty(), "{muxer}: no FLV output");
    let mut cursor = 13;
    let mut timestamps = Vec::new();
    while let Some(header) = parse_tag_header(&output[cursor.min(output.len())..]) {
      assert!(output.len() - cursor >= header.total_len());
      timestamps.push(header.timestamp_ms);
      cursor += header.total_len();
    }
    assert_eq!(cursor, output.len(), "{muxer}: invalid FLV framing");
    assert!(timestamps.windows(2).all(|pair| pair[0] <= pair[1]));
    assert!(
      timestamps.contains(&100),
      "{muxer}: delayed word was not emitted at its original timestamp"
    );

    if Command::new("ffmpeg")
      .arg("-version")
      .stdout(Stdio::null())
      .stderr(Stdio::null())
      .status()
      .is_ok_and(|status| status.success())
    {
      let mut child = Command::new("ffmpeg")
        .args([
          "-hide_banner",
          "-v",
          "error",
          "-f",
          "flv",
          "-i",
          "pipe:0",
          "-map",
          "0:s:0",
          "-f",
          "srt",
          "pipe:1",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
      child.stdin.as_mut().unwrap().write_all(&output).unwrap();
      let ffmpeg = child.wait_with_output().unwrap();
      assert!(
        String::from_utf8_lossy(&ffmpeg.stdout).contains("hello"),
        "{muxer}: FFmpeg did not demux the strict caption"
      );
    }
  }
}

/// A non-1x segment must be refused rather than silently mistimed.
///
/// Running time is scaled by `1 / |rate|`, so buffer durations do not survive
/// the conversion unchanged and every derived tag timestamp would be wrong.
#[test]
fn a_non_unity_segment_rate_is_rejected() {
  let mut harness = Harness::new();
  assert!(harness.push_segment_with_rate("text", 2.0));
  harness.push_gap(0, 500);
  harness.push_flv(0);
  assert!(
    harness.posted_error(),
    "segment rate 2.0 was accepted instead of failing the stream"
  );
}

/// A reverse-rate segment inverts intervals and must be refused.
#[test]
fn a_reverse_segment_rate_is_rejected() {
  let mut harness = Harness::new();
  assert!(harness.push_segment_with_rate("text", -1.0));
  harness.push_gap(0, 500);
  harness.push_flv(0);
  assert!(
    harness.posted_error(),
    "a reverse segment rate was accepted instead of failing the stream"
  );
}

/// A timestamp outside the segment is real information, not something to
/// approximate with the raw PTS.
#[test]
fn media_outside_its_segment_is_rejected() {
  let mut harness = Harness::new();
  harness.push_gap(0, 500);

  // A segment starting at 1s makes an earlier media timestamp out of range.
  let mut segment = gst::FormattedSegment::<gst::ClockTime>::new();
  segment.set_start(gst::ClockTime::from_seconds(1));
  assert!(harness.flv.push_event(gst::event::Segment::new(&segment)));
  harness.push_flv(0);

  assert!(
    harness.posted_error(),
    "media outside its segment was silently treated as raw PTS"
  );
}

/// A text timeline restart must not leave stale text on screen.
///
/// The cue from the old timeline is still displayed downstream, so the
/// element owes an explicit clear before anything from the new timeline.
#[test]
fn a_text_segment_clears_a_cue_left_on_screen() {
  let mut harness = Harness::new();
  harness
    .flv
    .element()
    .unwrap()
    .set_property_from_str("input-mode", "replacement");

  // A cue that stays active for the rest of the timeline.
  harness.push_text(0, 100, "stale");
  harness.push_flv(0);
  assert_eq!(
    harness.output_headers(2),
    vec![(0, TAG_TYPE_SCRIPT_DATA), (0, 9)]
  );

  // A new text segment declares a new caption timeline.
  harness
    .text
    .push_event(gst::event::Segment::new(&gst::FormattedSegment::<
      gst::ClockTime,
    >::new()));
  harness.push_gap(0, 500);
  harness.push_flv(100);

  // Drained with a bounded wait: without the clear cue only the media tag
  // arrives, and a blocking pull would hang instead of failing.
  std::thread::sleep(std::time::Duration::from_millis(100));
  assert_eq!(
    harness.drain_headers(),
    vec![(100, TAG_TYPE_SCRIPT_DATA), (100, 9)],
    "no clear cue was written after the text timeline restarted"
  );
}
