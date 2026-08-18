// SPDX-License-Identifier: MPL-2.0

//! Pure roll-up window: completed lines freeze; only the bottom line grows.

use std::collections::VecDeque;

use unicode_width::UnicodeWidthStr;

/// A window of lines. Completed lines are immutable; only `current` grows.
#[derive(Debug, Clone)]
pub struct Window {
    pub columns: usize,
    lines: usize,
    break_on_sentence: bool,
    done: VecDeque<String>,
    current: Vec<String>,
}

/// Result of pushing a word into the window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Push {
    /// The rendered window changed; redraw.
    Redraw(String),
    /// Nothing visible changed (reserved).
    Unchanged,
}

impl Window {
    pub fn new(columns: usize, lines: usize, break_on_sentence: bool) -> Self {
        Self {
            columns: columns.max(1),
            lines: lines.max(1),
            break_on_sentence,
            done: VecDeque::new(),
            current: Vec::new(),
        }
    }

    pub fn set_columns(&mut self, columns: usize) {
        self.columns = columns.max(1);
    }

    pub fn set_lines(&mut self, lines: usize) {
        self.lines = lines.max(1);
    }

    pub fn set_break_on_sentence(&mut self, break_on_sentence: bool) {
        self.break_on_sentence = break_on_sentence;
    }

    pub fn is_empty(&self) -> bool {
        self.done.is_empty() && self.current.is_empty()
    }

    pub fn clear(&mut self) {
        self.done.clear();
        self.current.clear();
    }

    /// Last `lines` entries of `done ++ [current]`, joined by `\\n`.
    pub fn render(&self) -> String {
        let mut entries: Vec<String> = self.done.iter().cloned().collect();
        if !self.current.is_empty() {
            entries.push(self.current.join(" "));
        }
        let start = entries.len().saturating_sub(self.lines);
        entries[start..].join("\n")
    }

    pub fn push_word(&mut self, word: &str) -> Push {
        let word = word.trim();
        if word.is_empty() {
            return Push::Unchanged;
        }

        if !self.current.is_empty() {
            let candidate = format!("{} {}", self.current.join(" "), word);
            if display_width(&candidate) > self.columns {
                self.freeze_current();
            }
        }

        self.current.push(word.to_string());

        if self.break_on_sentence && ends_sentence(word) {
            self.freeze_current();
        }

        let rendered = self.render();
        if rendered.is_empty() {
            Push::Unchanged
        } else {
            Push::Redraw(rendered)
        }
    }

    fn freeze_current(&mut self) {
        if self.current.is_empty() {
            return;
        }
        self.done.push_back(self.current.join(" "));
        self.current.clear();
        while self.done.len() > self.lines {
            self.done.pop_front();
        }
    }
}

fn display_width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

/// Trailing quotes and brackets are skipped so `he said "no."` still counts.
pub fn ends_sentence(text: &str) -> bool {
    text.trim_end()
        .trim_end_matches(['"', '\'', ')', ']', '}', '\u{201d}', '\u{2019}', '\u{00bb}'])
        .ends_with(['.', '!', '?', '\u{2026}'])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraps_and_freezes_completed_lines() {
        let mut w = Window::new(10, 2, true);
        assert_eq!(w.push_word("hello"), Push::Redraw("hello".into()));
        assert_eq!(w.push_word("world"), Push::Redraw("hello\nworld".into()));
        assert_eq!(w.push_word("again"), Push::Redraw("world\nagain".into()));
    }

    /// The property that distinguishes this from `textwrap`: a line, once
    /// completed, is never rewritten.
    ///
    /// Checked two ways, because a reflow shows up as both. Rewriting a line
    /// makes its words appear in two different frozen lines, so the frozen
    /// lines stop reproducing the input; and it makes a line vanish and come
    /// back, so its run of cues stops being contiguous. A global re-wrap - what
    /// `textwrap` does - fails on both counts.
    #[test]
    fn fixation_completed_lines_are_never_rewritten() {
        let words: Vec<&str> = "one of the things that is great about filming this \
                                here is that we get to see the work happening"
            .split_whitespace()
            .collect();

        let mut w = Window::new(20, 3, false);
        let mut cues = Vec::new();
        for word in &words {
            if let Push::Redraw(text) = w.push_word(word) {
                cues.push(text);
            }
        }
        assert!(cues.iter().any(|c| c.lines().count() == 3), "never filled");

        // Distinct frozen lines, in order of first appearance.
        let mut history: Vec<String> = Vec::new();
        for cue in &cues {
            for line in cue.lines().rev().skip(1) {
                if !history.iter().any(|seen| seen == line) {
                    history.push(line.to_string());
                }
            }
        }

        let bottom = cues.last().unwrap().lines().last().unwrap();
        let rebuilt: Vec<&str> = history
            .iter()
            .flat_map(|line| line.split_whitespace())
            .chain(bottom.split_whitespace())
            .collect();
        assert_eq!(rebuilt, words, "frozen lines do not reproduce the input");

        for line in &history {
            let hits: Vec<usize> = cues
                .iter()
                .enumerate()
                .filter(|(_, cue)| cue.lines().rev().skip(1).any(|l| l == line))
                .map(|(index, _)| index)
                .collect();
            assert_eq!(
                hits.last().unwrap() - hits.first().unwrap() + 1,
                hits.len(),
                "line {line:?} was displaced and came back: {hits:?}"
            );
        }
    }

    #[test]
    fn sentence_end_finishes_the_line() {
        let mut w = Window::new(40, 2, true);
        assert_eq!(w.push_word("Hello"), Push::Redraw("Hello".into()));
        assert_eq!(w.push_word("world."), Push::Redraw("Hello world.".into()));
        assert_eq!(
            w.push_word("Next"),
            Push::Redraw("Hello world.\nNext".into())
        );
    }

    #[test]
    fn overlong_word_keeps_its_own_line() {
        let mut w = Window::new(5, 2, false);
        assert_eq!(w.push_word("toolong"), Push::Redraw("toolong".into()));
        assert_eq!(w.push_word("ok"), Push::Redraw("toolong\nok".into()));
    }

    #[test]
    fn cjk_uses_display_width() {
        let mut w = Window::new(4, 2, false);
        // Each CJK glyph is two columns.
        assert_eq!(w.push_word("你好"), Push::Redraw("你好".into()));
        assert_eq!(w.push_word("啊"), Push::Redraw("你好\n啊".into()));
    }

    /// The bottom line only ever grows by exactly the word just pushed, or
    /// restarts as that word alone. Nothing else may change it.
    #[test]
    fn bottom_line_only_grows_by_the_pushed_word() {
        let words = ["alpha", "beta", "gamma", "delta", "epsilon"];
        let mut w = Window::new(10, 2, false);
        let mut previous = String::new();

        for word in words {
            let Push::Redraw(text) = w.push_word(word) else {
                panic!("{word} produced no redraw");
            };
            let bottom = text.lines().last().unwrap().to_string();
            let grew = !previous.is_empty() && bottom == format!("{previous} {word}");
            let restarted = bottom == word;
            assert!(
                grew || restarted,
                "bottom went from {previous:?} to {bottom:?} on {word:?}"
            );
            previous = bottom;
        }
    }

    #[test]
    fn height_never_exceeds_lines() {
        let mut w = Window::new(8, 2, false);
        for word in ["a", "bbbbbbb", "c", "ddddddd", "e", "fffffff"] {
            if let Push::Redraw(text) = w.push_word(word) {
                assert!(text.lines().count() <= 2, "{text}");
            }
        }
    }

    #[test]
    fn ends_sentence_skips_trailing_quotes() {
        assert!(ends_sentence("done."));
        assert!(ends_sentence("really?"));
        assert!(ends_sentence("wow!"));
        assert!(ends_sentence("\"no.\""));
        assert!(ends_sentence("trailing. "));
        assert!(!ends_sentence("poets,"));
        assert!(!ends_sentence("mid"));
    }
}
