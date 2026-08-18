// SPDX-License-Identifier: MPL-2.0

use gsttextrollup::rollup::window::{Push, Window};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct Cue {
    start: u64,
    end: u64,
    text: String,
}

#[derive(Debug, Deserialize)]
struct Word {
    pts: u64,
    #[allow(dead_code)]
    dur: u64,
    text: String,
}

#[derive(Debug, Deserialize)]
struct Fixture {
    words: Vec<Word>,
    cues: Vec<Cue>,
}

fn spans(words: &[Word]) -> Vec<(u64, u64, &str)> {
    let mut out = Vec::new();
    for (i, w) in words.iter().enumerate() {
        let text = w.text.trim();
        if text.is_empty() {
            continue;
        }
        let start = w.pts;
        let end = if i + 1 < words.len() {
            words[i + 1].pts
        } else {
            w.pts + w.dur.max(500_000_000)
        };
        if end <= start {
            continue;
        }
        out.push((start, end, text));
    }
    out
}

/// Mirror mkhls.py build_window(words, per_line=False, window_lines=2).
fn build_window(words: &[Word], columns: usize, window_lines: usize) -> Vec<(u64, u64, String)> {
    let mut cues = Vec::new();
    let mut window = Window::new(columns, window_lines, true);

    for (start, end, text) in spans(words) {
        match window.push_word(text) {
            Push::Redraw(rendered) => cues.push((start, end, rendered)),
            Push::Unchanged => {}
        }
    }

    // Each cue runs until the next begins.
    let mut fixed = Vec::new();
    for i in 0..cues.len() {
        let (s, e, t) = &cues[i];
        let end = if i + 1 < cues.len() {
            cues[i + 1].0
        } else {
            *e
        };
        if end <= *s {
            continue;
        }
        fixed.push((*s, end, t.clone()));
    }
    fixed
}

#[test]
fn oracle_matches_python_build_window() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/fixtures/oracle_window_lines2.json"
    );
    let raw = std::fs::read_to_string(path).expect("oracle fixture");
    let fixture: Fixture = serde_json::from_str(&raw).expect("parse oracle fixture");

    let got = build_window(&fixture.words, 42, 2);
    assert_eq!(got.len(), fixture.cues.len(), "cue count mismatch");

    for (i, ((gs, ge, gt), want)) in got.iter().zip(fixture.cues.iter()).enumerate() {
        assert_eq!(*gs, want.start, "cue {i} start");
        assert_eq!(*ge, want.end, "cue {i} end");
        assert_eq!(
            gt, &want.text,
            "cue {i} text\n got: {gt:?}\nwant: {:?}",
            want.text
        );
    }
}
