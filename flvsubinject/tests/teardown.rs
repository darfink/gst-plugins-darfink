// SPDX-License-Identifier: MPL-2.0

//! What happens to queued cues when the stream ends or is flushed.
//!
//! Both cases are invisible to an end-to-end caption count: a lost final cue
//! still leaves dozens of correct ones, and a flush mid-stream is rare enough
//! in live RTMP that a soak test would not reliably provoke it. They are
//! asserted directly instead.

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

/// Drive the element through `gst_check`, so cue delivery can be sequenced
/// against EOS precisely rather than raced through a live pipeline.
struct Harness {
  flv: gst_check::Harness,
  text: gst_check::Harness,
  collected: Vec<u8>,
}

impl Harness {
  fn new() -> Self {
    Self::new_with_mode(Some("replacement"))
  }

  fn new_with_mode(input_mode: Option<&str>) -> Self {
    init();
    let mut flv = gst_check::Harness::with_padnames("flvsubinject", Some("sink"), Some("src"));
    {
      let element = flv.element().unwrap();
      element.set_property("prime", false);
      if let Some(input_mode) = input_mode {
        element.set_property_from_str("input-mode", input_mode);
      }
    }

    // A second harness on the text pad of the *same* element, which is how
    // gst_check models an element with more than one sink pad.
    let mut text = gst_check::Harness::with_element(
      &flv.element().unwrap(),
      Some("text"),
      None,
    );

    flv.set_src_caps_str("video/x-flv");
    text.set_src_caps_str("text/x-raw, format=utf8");
    flv.play();

    Self {
      flv,
      text,
      collected: Vec::new(),
    }
  }

  /// Push a minimal but well-formed FLV video tag at `millis`.
  fn push_flv_tag(&mut self, millis: u64) {
    let body = [0x17u8, 0x01, 0x00, 0x00, 0x00];
    let mut tag = vec![9u8];
    tag.extend_from_slice(&(body.len() as u32).to_be_bytes()[1..]);
    let timestamp = millis as u32;
    tag.extend_from_slice(&timestamp.to_be_bytes()[1..]);
    tag.push((timestamp >> 24) as u8);
    tag.extend_from_slice(&[0, 0, 0]);
    tag.extend_from_slice(&body);
    let total = (11 + body.len()) as u32;
    tag.extend_from_slice(&total.to_be_bytes());

    let mut buffer = gst::Buffer::from_mut_slice(tag);
    {
      let buffer = buffer.get_mut().unwrap();
      buffer.set_pts(gst::ClockTime::from_mseconds(millis));
      buffer.set_duration(gst::ClockTime::from_mseconds(100));
    }
    self.flv.push(buffer).expect("push flv tag");
  }

  fn push_cue(&mut self, millis: u64, text: &str) {
    self.push_cue_with_duration(millis, 100, text);
  }

  fn push_cue_with_duration(&mut self, millis: u64, duration_ms: u64, text: &str) {
    let mut buffer = gst::Buffer::from_slice(text.as_bytes().to_vec());
    {
      let buffer = buffer.get_mut().unwrap();
      buffer.set_pts(gst::ClockTime::from_mseconds(millis));
      buffer.set_duration(gst::ClockTime::from_mseconds(duration_ms));
    }
    self.text.push(buffer).expect("push cue");
  }

  /// Cue texts pulled from the element so far, in order.
  ///
  /// Returning the payloads rather than a count is what makes a flush
  /// assertion meaningful: the question is *which* cue survived, and a count
  /// cannot distinguish a stale cue from its replacement.
  fn cue_texts(&mut self) -> Vec<String> {
    self.cue_states()
      .into_iter()
      .map(|(_, text)| text)
      .collect()
  }

  fn cue_states(&mut self) -> Vec<(u32, String)> {
    let mut bytes = Vec::new();
    while let Some(buffer) = self.flv.try_pull() {
      let map = buffer.map_readable().unwrap();
      bytes.extend_from_slice(map.as_slice());
    }
    self.collected.extend_from_slice(&bytes);
    let bytes = self.collected.clone();
    let mut cursor = 0usize;
    let mut texts = Vec::new();
    while let Some(header) = parse_tag_header(&bytes[cursor.min(bytes.len())..]) {
      if bytes.len() - cursor < header.total_len() {
        break;
      }
      if header.tag_type == TAG_TYPE_SCRIPT_DATA {
        let body = &bytes[cursor + 11..cursor + 11 + header.body_size];
        texts.push((header.timestamp_ms, extract_text(body)));
      }
      cursor += header.total_len();
    }
    texts
  }

  fn script_tag_count(&mut self) -> usize {
    self.cue_texts().len()
  }
}

#[test]
fn timed_mode_shows_then_clears_at_duration_end() {
  let mut harness = Harness::new_with_mode(None);
  harness.push_flv_tag(0);
  harness.push_cue_with_duration(100, 200, "timed");
  harness.push_flv_tag(100);
  harness.push_flv_tag(300);
  assert_eq!(
    harness.cue_states(),
    vec![(100, "timed".into()), (300, String::new())]
  );
}

#[test]
fn adjacent_timed_replacement_has_no_intermediate_clear() {
  let mut harness = Harness::new_with_mode(Some("timed"));
  harness.push_flv_tag(0);
  harness.push_cue_with_duration(100, 100, "first");
  harness.push_cue_with_duration(200, 100, "second");
  harness.push_flv_tag(100);
  harness.push_flv_tag(200);
  harness.push_flv_tag(300);
  assert_eq!(
    harness.cue_states(),
    vec![
      (100, "first".into()),
      (200, "second".into()),
      (300, String::new())
    ]
  );
}

#[test]
fn overlapping_old_clear_cannot_erase_newer_cue() {
  let mut harness = Harness::new_with_mode(Some("timed"));
  harness.push_flv_tag(0);
  harness.push_cue_with_duration(100, 200, "first");
  harness.push_cue_with_duration(200, 200, "second");
  for timestamp in [100, 200, 300, 400] {
    harness.push_flv_tag(timestamp);
  }
  assert_eq!(
    harness.cue_states(),
    vec![
      (100, "first".into()),
      (200, "second".into()),
      (400, String::new())
    ]
  );
}

#[test]
fn identical_contiguous_timed_text_extends_without_duplicate() {
  let mut harness = Harness::new_with_mode(Some("timed"));
  harness.push_flv_tag(0);
  harness.push_cue_with_duration(100, 100, "same");
  harness.push_cue_with_duration(200, 100, "same");
  for timestamp in [100, 200, 300] {
    harness.push_flv_tag(timestamp);
  }
  assert_eq!(
    harness.cue_states(),
    vec![(100, "same".into()), (300, String::new())]
  );
}

#[test]
fn replacement_mode_ignores_duration_until_explicit_clear() {
  let mut harness = Harness::new();
  harness.push_flv_tag(0);
  harness.push_cue_with_duration(100, 10, "persistent");
  harness.push_flv_tag(100);
  harness.push_flv_tag(1000);
  harness.push_cue_with_duration(1200, 0, "");
  harness.push_flv_tag(1200);
  assert_eq!(
    harness.cue_states(),
    vec![(100, "persistent".into()), (1200, String::new())]
  );
}

#[test]
fn same_timestamp_replacements_serialize_only_final_state() {
  let mut harness = Harness::new();
  harness.push_flv_tag(0);
  harness.push_cue(100, "first");
  harness.push_cue(100, "final");
  harness.push_flv_tag(100);
  assert_eq!(harness.cue_states(), vec![(100, "final".into())]);
}

#[test]
fn text_input_requires_pts_and_duration() {
  let mut harness = Harness::new();
  let mut missing_duration = gst::Buffer::from_slice(b"text".to_vec());
  missing_duration
    .get_mut()
    .unwrap()
    .set_pts(gst::ClockTime::from_mseconds(100));
  assert_eq!(
    harness.text.push(missing_duration),
    Err(gst::FlowError::Error)
  );

  let mut missing_pts = gst::Buffer::from_slice(b"text".to_vec());
  missing_pts
    .get_mut()
    .unwrap()
    .set_duration(gst::ClockTime::from_mseconds(100));
  assert_eq!(harness.text.push(missing_pts), Err(gst::FlowError::Error));
}

/// Pull the `text` property out of a serialized script-data body.
fn extract_text(body: &[u8]) -> String {
  let mut cursor = 1usize;
  let name_len = u16::from_be_bytes([body[cursor], body[cursor + 1]]) as usize;
  cursor += 2 + name_len + 1 + 4;
  loop {
    if cursor + 2 > body.len() {
      return String::new();
    }
    let property_len = u16::from_be_bytes([body[cursor], body[cursor + 1]]) as usize;
    cursor += 2;
    if property_len == 0 {
      return String::new();
    }
    let property = String::from_utf8_lossy(&body[cursor..cursor + property_len]).into_owned();
    cursor += property_len;
    let kind = body[cursor];
    cursor += 1;
    if kind == 0x02 {
      let len = u16::from_be_bytes([body[cursor], body[cursor + 1]]) as usize;
      cursor += 2;
      let value = String::from_utf8_lossy(&body[cursor..cursor + len]).into_owned();
      cursor += len;
      if property == "text" {
        return value;
      }
    } else {
      cursor += 8;
    }
  }
}

#[test]
fn transitions_past_final_media_are_discarded_at_eos() {
  let mut harness = Harness::new();

  harness.push_flv_tag(0);
  harness.push_flv_tag(100);
  // Ahead of the stream position, so it cannot be written by the normal path.
  harness.push_cue(5_000, "final caption");
  assert_eq!(
    harness.script_tag_count(),
    0,
    "a cue ahead of the stream must wait"
  );

  harness.flv.push_event(gst::event::Eos::new());

  assert_eq!(
    harness.script_tag_count(),
    0,
    "future transitions must not be clamped onto final media"
  );
}

#[test]
fn a_flush_discards_queued_cues_and_resets_the_timeline() {
  let mut harness = Harness::new();

  harness.push_flv_tag(0);
  harness.push_flv_tag(1_000);
  // Queued behind a stream that has already advanced past it, so it is
  // eligible to be written the moment any tag arrives. That is what makes the
  // assertion discriminating: without the flush reset, the cue survives and is
  // emitted against the new timeline.
  harness.push_cue(2_000, "stale");
  // Drain what the pre-flush stream produced, so the assertion below can only
  // see tags written after the flush.
  let _ = harness.cue_texts();

  harness.flv.push_event(gst::event::FlushStart::new());
  harness.flv.push_event(gst::event::FlushStop::new(true));
  harness
    .flv
    .push_event(gst::event::Segment::new(&gst::FormattedSegment::<
      gst::ClockTime,
    >::new()));

  // The pre-flush cue belongs to a timeline that is no longer being sent.
  harness.push_flv_tag(0);
  harness.push_flv_tag(100);

  // And the element must still work afterwards.
  harness.push_cue(0, "after flush");
  harness.push_flv_tag(200);
  // A tag beyond the stale cue's timestamp: if the cue survived the flush, this
  // is where it would be written.
  harness.push_flv_tag(3_000);

  let texts = harness.cue_texts();
  assert!(
    !texts.iter().any(|text| text == "stale"),
    "a cue from before the flush was written against the new timeline: {texts:?}"
  );
  assert!(
    texts.iter().any(|text| text == "after flush"),
    "cues after a flush must still be written: {texts:?}"
  );
}
