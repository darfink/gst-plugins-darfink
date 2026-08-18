// SPDX-License-Identifier: MPL-2.0

use gst::prelude::*;
use std::time::{Duration, Instant};

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

fn push_word(harness: &mut gst_check::Harness, pts_ms: u64, duration_ms: u64, text: &str) {
    let mut buffer = gst::Buffer::from_mut_slice(text.as_bytes().to_vec());
    {
        let buffer = buffer.get_mut().unwrap();
        buffer.set_pts(ms(pts_ms));
        buffer.set_duration(ms(duration_ms));
    }
    assert_eq!(harness.push(buffer), Ok(gst::FlowSuccess::Ok));
}

#[derive(Debug)]
enum Out {
    Cue(u64, u64, String),
    Gap(u64, u64),
}

fn drain(harness: &mut gst_check::Harness) -> Vec<Out> {
    let mut output = Vec::new();
    while let Some(event) = harness.try_pull_event() {
        if let gst::EventView::Gap(gap) = event.view() {
            let (pts, duration) = gap.get();
            let duration = duration.unwrap_or(gst::ClockTime::ZERO);
            output.push(Out::Gap(pts.mseconds(), (pts + duration).mseconds()));
        }
    }
    while let Some(buffer) = harness.try_pull() {
        let pts = buffer.pts().unwrap();
        let duration = buffer.duration().unwrap_or(gst::ClockTime::ZERO);
        let map = buffer.map_readable().unwrap();
        output.push(Out::Cue(
            pts.mseconds(),
            (pts + duration).mseconds(),
            std::str::from_utf8(map.as_slice()).unwrap().to_owned(),
        ));
    }
    output
}

fn gapkeeper_harness(properties: &[(&str, u32)]) -> gst_check::Harness {
    let mut harness = gst_check::Harness::new("gapkeeper");
    for (name, value) in properties {
        harness.element().unwrap().set_property(name, *value);
    }
    harness.set_src_caps_str("text/x-raw, format=utf8");
    harness
}

#[test]
fn gapkeeper_covers_silence_without_touching_buffers() {
    init();
    let mut harness = gapkeeper_harness(&[("keepalive-ms", 100)]);
    harness.play();
    let mut segment = gst::Segment::new();
    segment.set_format(gst::Format::Time);
    segment.set_start(ms(0));
    assert!(harness.push_event(gst::event::Segment::new(&segment)));
    push_word(&mut harness, 1000, 300, "Hello");

    let mut output = Vec::new();
    let deadline = Instant::now() + Duration::from_millis(800);
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(200));
        output.extend(drain(&mut harness));
    }

    let cues: Vec<_> = output
        .iter()
        .filter_map(|item| match item {
            Out::Cue(start, end, text) => Some((*start, *end, text.clone())),
            Out::Gap(_, _) => None,
        })
        .collect();
    assert_eq!(cues, vec![(1000, 1300, "Hello".to_owned())]);
    let gaps: Vec<_> = output
        .iter()
        .filter_map(|item| match item {
            Out::Gap(start, end) => Some((*start, *end)),
            Out::Cue(_, _, _) => None,
        })
        .collect();
    assert!(gaps.iter().all(|(start, end)| start == end));
    assert!(gaps.iter().any(|(start, _)| *start >= 1000));
}

#[test]
fn gapkeeper_stall_advances_the_frontier() {
    init();
    let mut harness = gapkeeper_harness(&[("keepalive-ms", 100), ("stall-ms", 500)]);
    harness.play();
    let mut segment = gst::Segment::new();
    segment.set_format(gst::Format::Time);
    segment.set_start(ms(0));
    assert!(harness.push_event(gst::event::Segment::new(&segment)));
    push_word(&mut harness, 1000, 100, "Hello");

    let mut output = Vec::new();
    let deadline = Instant::now() + Duration::from_millis(2200);
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(250));
        output.extend(drain(&mut harness));
    }
    let furthest = output
        .iter()
        .filter_map(|item| match item {
            Out::Gap(_, end) => Some(*end),
            Out::Cue(_, _, _) => None,
        })
        .max()
        .unwrap_or(0);
    assert!(furthest > 1000, "frontier only reached {furthest}ms");
}
