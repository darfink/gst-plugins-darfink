// SPDX-License-Identifier: MPL-2.0

use gst::prelude::*;

fn init() {
    use std::sync::Once;
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        gst::init().unwrap();
        gsttextrollup::plugin_register_static().expect("textrollup test");
    });
}

fn ms(value: u64) -> gst::ClockTime {
    gst::ClockTime::from_mseconds(value)
}

fn harness(clear_after: u32) -> gst_check::Harness {
    init();
    let mut harness = gst_check::Harness::new("textrollup");
    harness
        .element()
        .unwrap()
        .set_property("clear-after", clear_after);
    harness.set_src_caps_str("text/x-raw, format=utf8");
    harness
}

fn buffer(pts_ms: u64, duration_ms: Option<u64>, text: &str) -> gst::Buffer {
    let mut buffer = gst::Buffer::from_mut_slice(text.as_bytes().to_vec());
    {
        let buffer = buffer.get_mut().unwrap();
        buffer.set_pts(ms(pts_ms));
        if let Some(duration_ms) = duration_ms {
            buffer.set_duration(ms(duration_ms));
        }
    }
    buffer
}

fn push(harness: &mut gst_check::Harness, pts_ms: u64, duration_ms: u64, text: &str) {
    assert_eq!(
        harness.push(buffer(pts_ms, Some(duration_ms), text)),
        Ok(gst::FlowSuccess::Ok)
    );
}

fn pull(harness: &mut gst_check::Harness) -> (u64, u64, String) {
    let buffer = harness.pull().expect("caption state");
    let map = buffer.map_readable().unwrap();
    (
        buffer.pts().unwrap().mseconds(),
        buffer.duration().unwrap().mseconds(),
        std::str::from_utf8(map.as_slice()).unwrap().to_owned(),
    )
}

fn push_gap(harness: &mut gst_check::Harness, start_ms: u64, duration_ms: u64) {
    assert!(
        harness.push_event(
            gst::event::Gap::builder(ms(start_ms))
                .duration(ms(duration_ms))
                .build()
        )
    );
}

fn pull_gaps(harness: &mut gst_check::Harness) -> Vec<(u64, u64)> {
    let mut gaps = Vec::new();
    while let Some(event) = harness.try_pull_event() {
        if let gst::EventView::Gap(gap) = event.view() {
            let (start, duration) = gap.get();
            gaps.push((start.mseconds(), duration.unwrap().mseconds()));
        }
    }
    gaps
}

#[test]
fn first_word_is_immediate_and_preserves_timing() {
    let mut harness = harness(3000);
    push(&mut harness, 1000, 83, "Hello");
    assert_eq!(pull(&mut harness), (1000, 83, "Hello".into()));
}

#[test]
fn every_committed_buffer_produces_one_complete_state() {
    let mut harness = harness(0);
    harness
        .element()
        .unwrap()
        .set_property("break-on-sentence", false);
    push(&mut harness, 1000, 80, "One");
    push(&mut harness, 1200, 90, "two words");
    assert_eq!(pull(&mut harness), (1000, 80, "One".into()));
    assert_eq!(pull(&mut harness), (1200, 90, "One two words".into()));
    assert!(harness.try_pull().is_none());
}

#[test]
fn requires_duration_and_nonzero_duration() {
    let mut harness = harness(0);
    assert_eq!(
        harness.push(buffer(1000, None, "word")),
        Err(gst::FlowError::Error)
    );
    assert_eq!(
        harness.push(buffer(1000, Some(0), "word")),
        Err(gst::FlowError::Error)
    );
}

#[test]
fn short_silence_preserves_the_window() {
    let mut harness = harness(3000);
    push(&mut harness, 1000, 100, "Hello");
    push_gap(&mut harness, 1100, 2500);
    push(&mut harness, 3600, 100, "again");
    assert_eq!(pull(&mut harness).2, "Hello");
    assert_eq!(pull(&mut harness).2, "Hello again");
}

#[test]
fn gap_crossing_deadline_splits_around_one_clear() {
    let mut harness = harness(3000);
    push(&mut harness, 1000, 100, "Hello");
    assert_eq!(pull(&mut harness).2, "Hello");
    push_gap(&mut harness, 1100, 4000);
    assert_eq!(pull(&mut harness), (4100, 0, String::new()));
    assert!(harness.try_pull().is_none(), "clear is emitted only once");
    assert_eq!(pull_gaps(&mut harness), vec![(1100, 3000), (4100, 1000)]);
}

#[test]
fn later_word_emits_missed_clear_at_media_deadline() {
    let mut harness = harness(3000);
    push(&mut harness, 1000, 100, "old");
    assert_eq!(pull(&mut harness).2, "old");
    push(&mut harness, 5000, 100, "new");
    assert_eq!(pull(&mut harness), (4100, 0, String::new()));
    assert_eq!(pull(&mut harness), (5000, 100, "new".into()));
}

#[test]
fn explicit_empty_input_clears_and_resets() {
    let mut harness = harness(0);
    push(&mut harness, 1000, 100, "old words");
    assert_eq!(pull(&mut harness).2, "old words");
    assert_eq!(
        harness.push(buffer(1500, None, "")),
        Ok(gst::FlowSuccess::Ok)
    );
    assert_eq!(pull(&mut harness), (1500, 0, String::new()));
    push(&mut harness, 2000, 100, "fresh");
    assert_eq!(pull(&mut harness).2, "fresh");
}

#[test]
fn clear_after_zero_never_generates_a_clear() {
    let mut harness = harness(0);
    push(&mut harness, 1000, 100, "old");
    assert_eq!(pull(&mut harness).2, "old");
    push_gap(&mut harness, 1100, 10_000);
    assert!(harness.try_pull().is_none());
}

#[test]
fn sentence_and_width_scrolls_never_emit_blank_state() {
    let mut harness = harness(0);
    {
        let element = harness.element().unwrap();
        element.set_property("columns", 12u32);
        element.set_property("lines", 2u32);
        assert!(element.property::<bool>("break-on-sentence"));
    }
    for (pts, text) in [
        (0, "Hello"),
        (100, "world."),
        (200, "Next"),
        (300, "longword"),
    ] {
        push(&mut harness, pts, 50, text);
    }
    let states: Vec<_> = (0..4).map(|_| pull(&mut harness).2).collect();
    assert_eq!(states[1], "Hello world.");
    assert_eq!(states[2], "Hello world.\nNext");
    assert!(states.iter().all(|state| !state.is_empty()));
}

#[test]
fn flush_stop_drops_the_old_window() {
    let mut harness = harness(0);
    push(&mut harness, 1000, 50, "stale");
    assert_eq!(pull(&mut harness).2, "stale");
    assert!(harness.push_event(gst::event::FlushStart::new()));
    assert!(harness.push_event(gst::event::FlushStop::new(true)));
    push(&mut harness, 2000, 50, "fresh");
    assert_eq!(pull(&mut harness).2, "fresh");
}

#[test]
fn latency_query_adds_no_formatter_latency() {
    init();
    let mut harness = gst_check::Harness::new_parse("identity ! textrollup name=r");
    harness.set_src_caps_str("text/x-raw, format=utf8");
    harness.play();
    let src = harness.element().unwrap().static_pad("src").unwrap();
    let mut query = gst::query::Latency::new();
    assert!(src.query(&mut query));
    let (_live, min, _max) = query.result();
    assert_eq!(min, gst::ClockTime::ZERO);
}

#[test]
fn repeated_runs_have_identical_media_time_output() {
    fn run() -> Vec<(u64, u64, String)> {
        let mut harness = harness(3000);
        push(&mut harness, 1000, 100, "one");
        push(&mut harness, 1200, 100, "two");
        push_gap(&mut harness, 1300, 4000);
        let mut output = Vec::new();
        while harness.buffers_in_queue() > 0 {
            output.push(pull(&mut harness));
        }
        output
    }
    assert_eq!(run(), run());
    assert_eq!(run().last().unwrap().0, 4300);
}

#[test]
fn removed_transport_workaround_properties_are_absent() {
    let harness = harness(3000);
    let element = harness.element().unwrap();
    for name in ["hold", "persist", "clear-timeout", "emit-clear-cue"] {
        assert!(element.find_property(name).is_none(), "{name} still exists");
    }
}
