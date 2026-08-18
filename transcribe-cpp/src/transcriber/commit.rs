// SPDX-License-Identifier: MPL-2.0

//! Turning a growing committed prefix into append-only timed words.
//!
//! The streaming API hands us two views of the same hypothesis:
//!
//! - [`StreamText::committed`], an append-only *character* prefix of the raw
//!   hypothesis. It never rewrites, so it is what we are allowed to push
//!   downstream.
//! - the timed rows from [`Stream::snapshot`] — word rows where the family
//!   aligns to words, segment rows otherwise (see [`units`]) — carrying
//!   `t0_ms` / `t1_ms`. These cover the whole hypothesis, tentative tail
//!   included, and may be rewritten anywhere.
//!
//! A timed buffer needs both, so we have to decide which rows the committed
//! prefix covers. Two independent signals are used, and a row must satisfy
//! both:
//!
//! 1. *Textual*: walking the rows in order, each row's text must be findable
//!    in the committed prefix at or after the previous match. Row text and the
//!    committed string do not always agree byte for byte (whitespace, and under
//!    [`CommitPolicy::Auto`] the family may normalize), so this is a forward
//!    scan rather than a length comparison.
//! 2. *Temporal*: the row must end at or before `audio_committed_ms`, the
//!    family's own report of how far the audio has drained. Under
//!    [`CommitPolicy::Auto`] this is the conservative of the two and keeps us
//!    from emitting text the family still considers in flight.
//!
//! [`StreamText::committed`]: transcribe_cpp::StreamText::committed
//! [`Transcript::words`]: transcribe_cpp::Transcript::words
//! [`Stream::snapshot`]: transcribe_cpp::Stream::snapshot
//! [`CommitPolicy::Auto`]: transcribe_cpp::CommitPolicy::Auto

use std::borrow::Cow;
use std::ops::Range;

use transcribe_cpp::{TimestampKind, Token, Transcript};

/// One timed unit ready to be pushed, in stream-relative milliseconds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimedWord {
    pub text: String,
    pub t0_ms: i64,
    pub t1_ms: i64,
}

/// A timed row from a transcript. Borrowed unless tokens had to be joined.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unit<'a> {
    pub text: Cow<'a, str>,
    pub t0_ms: i64,
    pub t1_ms: i64,
}

/// The finest timed rows a transcript actually carries.
///
/// Families disagree about what they align, and the disagreement is not a
/// preference we get to state — asking whisper for word timestamps is a hard
/// error, and the streaming families here populate only tokens no matter what
/// is requested. So take whatever is populated, best first:
///
/// - **words**, when the family aligns to words;
/// - **segments**, what whisper reports;
/// - **tokens**, joined back into words — what the streaming families report.
///
/// An empty result means the transcript carries no alignment at all.
pub fn units(transcript: &Transcript) -> Vec<Unit<'_>> {
    // Take the family at its word about whether it aligns anything. Rows being
    // *present* is not the same as their timestamps being real:
    // moonshine-streaming reports `None` yet still fills in a segment and
    // tokens, all timed at zero. Trusting those stamps the whole transcript at
    // the start of the stream.
    if transcript.timestamp_kind == TimestampKind::None {
        return Vec::new();
    }

    if !transcript.words.is_empty() {
        return transcript
            .words
            .iter()
            .map(|w| Unit {
                text: Cow::Borrowed(&w.text),
                t0_ms: w.t0_ms,
                t1_ms: w.t1_ms,
            })
            .collect();
    }

    if !transcript.segments.is_empty() {
        return transcript
            .segments
            .iter()
            .map(|s| Unit {
                text: Cow::Borrowed(&s.text),
                t0_ms: s.t0_ms,
                t1_ms: s.t1_ms,
            })
            .collect();
    }

    units_from_tokens(&transcript.tokens)
}

/// Join sub-word tokens back into words.
///
/// A token is a vocabulary piece, not a word — "Satya" arrives as `Sat` + `ya`
/// — and one buffer per piece would be useless downstream. Tokens carry a
/// `word_index`, so prefer that; fall back to the leading-space convention when
/// the family leaves it unset.
fn units_from_tokens(tokens: &[Token]) -> Vec<Unit<'_>> {
    let mut out: Vec<Unit<'_>> = Vec::new();
    let mut word_index = i32::MIN;
    let mut start_next = true;

    for token in tokens {
        if token.text.trim().is_empty() {
            // Carries no text of its own, but does end the current word.
            start_next = true;
            continue;
        }

        let starts_word = start_next
            || if token.word_index >= 0 {
                token.word_index != word_index
            } else {
                token.text.starts_with(char::is_whitespace)
            };

        match out.last_mut() {
            Some(last) if !starts_word => {
                last.text.to_mut().push_str(&token.text);
                last.t1_ms = last.t1_ms.max(token.t1_ms);
            }
            _ => out.push(Unit {
                text: Cow::Borrowed(token.text.as_str()),
                t0_ms: token.t0_ms,
                t1_ms: token.t1_ms,
            }),
        }

        word_index = token.word_index;
        start_next = false;
    }

    out
}

/// Tracks how much of a stream's committed text has already been emitted.
///
/// One instance per stream; drop it when the stream is reset or finalized.
#[derive(Debug, Default)]
pub struct CommitTracker {
    /// Number of word rows already emitted.
    emitted_words: usize,
    /// Bytes of the committed prefix already emitted. Only used by the
    /// untimed fallback, where there are no word rows to count.
    emitted_bytes: usize,
    /// End of the last emitted word, for synthesizing spans in the fallback.
    emitted_up_to_ms: i64,
    /// `(t0, t1)` of the first row not yet emitted, if any. Nothing may be
    /// published at or after `t0` until that row goes out, or it would be
    /// dragged forward to make room.
    pending: Option<(i64, i64)>,

    /// Length of the committed text when it was last seen to grow, and the
    /// family's audio edge at that moment. The settle margin is measured from
    /// here — see [`HOLDBACK_SETTLE_MS`].
    observed_len: usize,
    last_growth_ms: i64,
}

/// How much further the family must consume, without changing its mind about
/// the text, before the trailing-row holdback is released.
///
/// The holdback exists because a word can be committed as a character prefix -
/// "Ur" with "du" still to come. Without a release the last word before a pause
/// would be stranded until somebody spoke again, so a row does have to come out
/// eventually.
///
/// The margin is measured from the last time the committed text *grew*, not
/// from the row's own end timestamp. A row's timestamp is no evidence about its
/// text: these families routinely place a word's end well behind the audio
/// edge while still appending characters to it, so an anchor on `t1_ms` fires
/// while the word is mid-growth and publishes "import" for "important" — and,
/// because trailing punctuation arrives as its own late token, strips the
/// sentence-final period along with it.
const HOLDBACK_SETTLE_MS: i64 = 500;

impl CommitTracker {
    /// Words newly covered by `committed` that have not been emitted yet.
    ///
    /// `audio_committed_ms` is [`StreamUpdate::audio_committed_ms`]; pass 0 to
    /// skip the temporal check (the family does not report progress).
    ///
    /// [`StreamUpdate::audio_committed_ms`]: transcribe_cpp::StreamUpdate::audio_committed_ms
    pub fn take_new(
        &mut self,
        committed: &str,
        units: &[Unit],
        audio_committed_ms: i64,
    ) -> Vec<TimedWord> {
        self.take(committed, units, audio_committed_ms, false)
    }

    /// Everything left, ignoring both stability checks. For `finalize`.
    pub fn take_final(
        &mut self,
        committed: &str,
        units: &[Unit],
        audio_committed_ms: i64,
    ) -> Vec<TimedWord> {
        self.take(committed, units, audio_committed_ms, true)
    }

    fn take(
        &mut self,
        committed: &str,
        units: &[Unit],
        audio_committed_ms: i64,
        final_: bool,
    ) -> Vec<TimedWord> {
        // Committed text is append-only, so a length change is a growth.
        if committed.len() != self.observed_len {
            self.observed_len = committed.len();
            self.last_growth_ms = audio_committed_ms;
        }

        if units.is_empty() {
            return self.take_untimed(committed, audio_committed_ms);
        }

        let (aligned, spans) = align_units(committed, units);
        let covered = if final_ {
            units.len()
        } else {
            // The trailing row is not normally safe to emit. The family commits
            // *character* prefixes, so a word can be committed as "Ur" while
            // the rest of it is still coming: the next snapshot has tokens
            // `Ur` + `du`, which join into "Urdu" at the same index. Emitting
            // the trailing row now would publish "Ur" and swallow "du", since
            // that index is then marked done. Anything with a row after it has
            // provably ended, so hold back exactly one row and let the next
            // call emit it — one word of extra latency, whole words out.
            let n = aligned;
            // Unless no successor can arrive to release it. That is only true
            // when the covered rows are the whole snapshot: a row followed by
            // tentative content may still absorb it and grow. Waiting on a
            // successor that silence will never provide is what stranded the
            // last word of every utterance.
            let settled = n > 0
                && n == units.len()
                && audio_committed_ms > 0
                && self.text_has_settled(audio_committed_ms);
            if settled { n } else { n.saturating_sub(1) }
        };

        // Re-segmentation can merge rows, so the covered count is not
        // guaranteed to be monotonic even though the committed text is.
        let start = self.emitted_words.min(covered);

        let mut out = Vec::new();
        for (offset, word) in units[start..covered].iter().enumerate() {
            // The family has not drained this far yet: stop, and pick the word
            // up on a later call. Timestamps are monotonic, so no later word
            // can qualify either.
            if !final_ && audio_committed_ms > 0 && word.t1_ms > audio_committed_ms {
                break;
            }

            let fallback = word.text.trim();
            self.emitted_words = start + offset + 1;
            if fallback.is_empty() {
                continue;
            }

            // Some model families put punctuation only in the canonical
            // committed transcript while their timed rows contain bare words.
            // Project the surrounding punctuation onto the row so downstream
            // keeps both the model's text and the row's timing.
            let text = canonical_unit_text(committed, &spans, start + offset, fallback);

            // Guard against a family that emits zero- or negative-length spans.
            let t1_ms = word.t1_ms.max(word.t0_ms);
            self.emitted_up_to_ms = self.emitted_up_to_ms.max(t1_ms);
            out.push(TimedWord {
                text,
                t0_ms: word.t0_ms.max(0),
                t1_ms,
            });
        }

        self.emitted_bytes = committed.len();
        self.pending = units
            .get(self.emitted_words)
            .map(|unit| (unit.t0_ms, unit.t1_ms));
        out
    }

    /// The first row still waiting to go out, as `(start, end)` milliseconds.
    #[cfg(test)]
    pub fn pending(&self) -> Option<(i64, i64)> {
        self.pending
    }

    /// Whether a row is waiting whose ambiguity the family has already
    /// resolved, so it can be released without waiting for a successor.
    pub fn pending_is_settled(&self, audio_committed_ms: i64) -> bool {
        self.pending.is_some() && self.text_has_settled(audio_committed_ms)
    }

    /// Whether the family has consumed [`HOLDBACK_SETTLE_MS`] of audio without
    /// appending anything, so the trailing row has provably stopped growing.
    fn text_has_settled(&self, audio_committed_ms: i64) -> bool {
        audio_committed_ms - self.last_growth_ms >= HOLDBACK_SETTLE_MS
    }

    /// How far the timeline may be closed, and record that it was.
    ///
    /// Closing past a pending row's start would force that row forward when it
    /// finally goes out, since output timestamps only move forward. So the
    /// frontier stops there, and reaches the family's committed edge only when
    /// nothing is waiting.
    ///
    /// A family with no alignment is excluded by the caller: its spans are
    /// synthesized from this same watermark and already tile the timeline, so
    /// there is nothing to fill.
    pub fn frontier(&self, audio_committed_ms: i64) -> i64 {
        match self.pending {
            Some((t0_ms, _)) => audio_committed_ms.min(t0_ms),
            None => audio_committed_ms,
        }
    }

    /// `timestamps=none`, or a family that produced no word rows: emit the new
    /// committed text as a single span ending at the family's drain point.
    fn take_untimed(&mut self, committed: &str, audio_committed_ms: i64) -> Vec<TimedWord> {
        if committed.len() <= self.emitted_bytes {
            return Vec::new();
        }

        // `committed` is append-only, but be defensive about a family that
        // rewrites it anyway: only trust the tail if the prefix still matches.
        let delta = if committed.is_char_boundary(self.emitted_bytes) {
            &committed[self.emitted_bytes..]
        } else {
            committed
        };
        let text = delta.trim();
        self.emitted_bytes = committed.len();
        if text.is_empty() {
            return Vec::new();
        }

        let t0_ms = self.emitted_up_to_ms;
        let t1_ms = if audio_committed_ms > t0_ms {
            audio_committed_ms
        } else {
            t0_ms
        };
        self.emitted_up_to_ms = t1_ms;

        vec![TimedWord {
            text: text.to_string(),
            t0_ms,
            t1_ms,
        }]
    }
}

/// Align timed rows with the canonical committed transcript.
///
/// The ranges let callers recover punctuation and capitalization without
/// inventing timestamps for characters that have no row of their own.
/// Scanning stops at the first row absent from the committed prefix, because
/// no later row can be committed either.
fn align_units(committed: &str, units: &[Unit]) -> (usize, Vec<Option<Range<usize>>>) {
    let mut cursor = 0usize;
    let mut spans = Vec::with_capacity(units.len());

    for (index, word) in units.iter().enumerate() {
        let needle = word.text.trim();
        if needle.is_empty() {
            spans.push(None);
            continue;
        }
        match committed[cursor..].find(needle) {
            Some(offset) => {
                let start = cursor + offset;
                let end = start + needle.len();
                spans.push(Some(start..end));
                cursor = end;
            }
            None => return (index, spans),
        }
    }

    (units.len(), spans)
}

/// Recover punctuation immediately surrounding one aligned timed row.
fn canonical_unit_text(
    committed: &str,
    spans: &[Option<Range<usize>>],
    index: usize,
    fallback: &str,
) -> String {
    let Some(range) = spans.get(index).and_then(Option::as_ref) else {
        return fallback.to_string();
    };

    let previous = spans[..index].iter().rev().find_map(Option::as_ref);
    let next = spans[index + 1..].iter().find_map(Option::as_ref);

    let before = &committed[previous.map_or(0, |span| span.end)..range.start];
    let leading = if previous.is_none() {
        before.rsplit(char::is_whitespace).next().unwrap_or(before)
    } else if before.chars().any(char::is_whitespace) {
        before.rsplit(char::is_whitespace).next().unwrap_or("")
    } else {
        ""
    };

    let after = &committed[range.end..next.map_or(committed.len(), |span| span.start)];
    let trailing = after.split(char::is_whitespace).next().unwrap_or(after);

    let leading = if punctuation_only(leading) {
        leading
    } else {
        ""
    };
    let trailing = if punctuation_only(trailing) {
        trailing
    } else {
        ""
    };
    format!("{leading}{}{trailing}", &committed[range.clone()])
}

fn punctuation_only(text: &str) -> bool {
    text.chars()
        .all(|character| !character.is_alphanumeric() && !character.is_control())
}

#[cfg(test)]
mod tests {
    use super::*;
    use transcribe_cpp::Segment;

    fn word(text: &str, t0_ms: i64, t1_ms: i64) -> Unit<'_> {
        Unit {
            text: Cow::Borrowed(text),
            t0_ms,
            t1_ms,
        }
    }

    fn token(text: &str, word_index: i32, t0_ms: i64, t1_ms: i64) -> Token {
        Token {
            id: 0,
            p: 1.0,
            t0_ms,
            t1_ms,
            seg_index: 0,
            word_index,
            text: text.to_string(),
        }
    }

    fn texts(words: &[TimedWord]) -> Vec<&str> {
        words.iter().map(|w| w.text.as_str()).collect()
    }

    fn transcript_with(kind: TimestampKind, segments: Vec<Segment>) -> Transcript {
        Transcript {
            timestamp_kind: kind,
            segments,
            ..Default::default()
        }
    }

    #[test]
    fn rows_are_ignored_when_the_family_reports_no_timestamps() {
        // moonshine-streaming fills in a segment while reporting None; its
        // times are all zero, so using it would stamp everything at 0.
        let rows = vec![Segment {
            text: "the whole transcript".into(),
            ..Default::default()
        }];

        assert!(units(&transcript_with(TimestampKind::None, rows.clone())).is_empty());
        assert_eq!(
            units(&transcript_with(TimestampKind::Segment, rows)).len(),
            1
        );
    }

    #[test]
    fn joins_sub_word_tokens_by_word_index() {
        // "Satya and" as the streaming families actually deliver it.
        let tokens = [
            token(" Sat", 0, 0, 200),
            token("ya", 0, 200, 400),
            token(" and", 1, 400, 600),
        ];

        let units = units_from_tokens(&tokens);
        assert_eq!(units.len(), 2);
        assert_eq!(units[0].text, " Satya");
        assert_eq!((units[0].t0_ms, units[0].t1_ms), (0, 400));
        assert_eq!(units[1].text, " and");
    }

    #[test]
    fn joins_tokens_by_leading_space_without_a_word_index() {
        let tokens = [
            token(" Sat", -1, 0, 200),
            token("ya", -1, 200, 400),
            token(" and", -1, 400, 600),
        ];

        let units = units_from_tokens(&tokens);
        assert_eq!(units.len(), 2);
        assert_eq!(units[0].text, " Satya");
        assert_eq!(units[1].text, " and");
    }

    #[test]
    fn a_whitespace_token_separates_words() {
        let tokens = [
            token("hello", -1, 0, 200),
            token(" ", -1, 200, 210),
            token("world", -1, 210, 400),
        ];

        let units = units_from_tokens(&tokens);
        assert_eq!(units.len(), 2);
        assert_eq!(units[0].text, "hello");
        assert_eq!(units[1].text, "world");
    }

    #[test]
    fn holds_back_a_word_that_is_still_growing() {
        let mut tracker = CommitTracker::default();

        // The family committed a character prefix that lands mid-word: the
        // snapshot's trailing row is "Ur", but the word is "Urdu".
        let units = [word("the", 0, 200), word("Ur", 200, 400)];
        let emitted = tracker.take_new("the Ur", &units, 400);
        assert_eq!(texts(&emitted), ["the"]);

        // Next snapshot joined the continuation onto the same row. Because we
        // never published "Ur", the whole word goes out now.
        let units = [
            word("the", 0, 200),
            word("Urdu", 200, 500),
            word("poetry", 500, 800),
        ];
        let emitted = tracker.take_new("the Urdu poetry", &units, 800);
        assert_eq!(texts(&emitted), ["Urdu"]);

        // And finalize releases the last one.
        let emitted = tracker.take_final("the Urdu poetry", &units, 800);
        assert_eq!(texts(&emitted), ["poetry"]);
    }

    #[test]
    fn releases_a_settled_row_without_waiting_for_a_successor() {
        let mut tracker = CommitTracker::default();
        let units = [word("the", 0, 200), word("Urdu", 200, 500)];

        // At the committed edge the trailing row is still ambiguous, so it
        // waits, exactly as it would mid-sentence.
        assert_eq!(texts(&tracker.take_new("the Urdu", &units, 500)), ["the"]);
        assert_eq!(tracker.pending(), Some((200, 500)));

        // Silence follows: no new text will ever arrive to push it out. Once the
        // family has finalized well past the row, the ambiguity is resolved and
        // it goes without a successor.
        assert!(tracker.pending_is_settled(1_000));
        assert_eq!(
            texts(&tracker.take_new("the Urdu", &units, 1_000)),
            ["Urdu"]
        );
        assert_eq!(tracker.pending(), None);
    }

    #[test]
    fn a_row_at_the_committed_edge_is_not_settled() {
        let mut tracker = CommitTracker::default();
        let units = [word("the", 0, 200), word("Ur", 200, 400)];
        tracker.take_new("the Ur", &units, 400);

        // Only 100ms past the row: still inside the window where the family may
        // yet extend "Ur" into "Urdu".
        assert!(!tracker.pending_is_settled(500));
    }

    /// These families place a word's end timestamp well behind the audio edge
    /// while still appending characters to it, so the row's own `t1_ms` is no
    /// evidence that its text is final. Anchoring the settle margin there
    /// published "import" for "important" and dropped sentence-final
    /// punctuation, which in turn stopped downstream seeing sentence ends.
    #[test]
    fn a_row_still_growing_is_not_settled_however_old_its_timestamp() {
        let mut tracker = CommitTracker::default();

        // The row ends at 400ms but the family is already 2s into the audio -
        // a huge margin by the row's own clock.
        let units = [word("all", 0, 200), word("import", 200, 400)];
        assert_eq!(
            texts(&tracker.take_new("all import", &units, 2_000)),
            ["all"]
        );
        assert!(
            !tracker.pending_is_settled(2_000),
            "text that just grew must not count as settled"
        );

        // It was still growing, and the next snapshot proves it.
        let units = [word("all", 0, 200), word("important", 200, 450)];
        assert!(tracker.take_new("all important", &units, 2_100).is_empty());

        // Now the family consumes half a second without appending anything.
        assert!(tracker.pending_is_settled(2_600));
        assert_eq!(
            texts(&tracker.take_new("all important", &units, 2_600)),
            ["important"]
        );
    }

    #[test]
    fn the_frontier_never_passes_a_pending_row() {
        let mut tracker = CommitTracker::default();
        let units = [word("the", 0, 200), word("Urdu", 200, 500)];
        tracker.take_new("the Urdu", &units, 500);

        // Closing the timeline at 500 would strand "Urdu", which starts at 200
        // and has not gone out yet.
        assert_eq!(tracker.frontier(500), 200);
    }

    #[test]
    fn the_frontier_follows_the_committed_edge_when_nothing_waits() {
        let mut tracker = CommitTracker::default();
        let units = [word("the", 0, 200)];

        // Finalizing empties the queue, so nothing constrains the timeline and
        // silence can be closed all the way to the committed edge.
        tracker.take_final("the", &units, 30_000);
        assert_eq!(tracker.pending(), None);
        assert_eq!(tracker.frontier(30_000), 30_000);
    }

    #[test]
    fn emits_each_word_once_as_the_prefix_grows() {
        let mut tracker = CommitTracker::default();
        let words = vec![word(" hello", 0, 500), word(" world", 500, 900)];

        // The trailing row is always held back, so one row alone emits nothing.
        assert!(tracker.take_new("hello", &words[..1], 500).is_empty());

        let first = tracker.take_new("hello world", &words, 900);
        assert_eq!(texts(&first), ["hello"]);
        assert_eq!(first[0].t0_ms, 0);
        assert_eq!(first[0].t1_ms, 500);

        // Same snapshot again: nothing new.
        assert!(tracker.take_new("hello world", &words, 900).is_empty());

        let second = tracker.take_final("hello world", &words, 900);
        assert_eq!(texts(&second), ["world"]);
    }

    #[test]
    fn withholds_words_the_prefix_does_not_cover_yet() {
        let mut tracker = CommitTracker::default();
        // The snapshot runs ahead of the committed prefix: only "hello" is
        // committed, so "world" and "again" are still tentative.
        let words = vec![
            word(" hello", 0, 500),
            word(" world", 500, 900),
            word(" again", 900, 1_200),
        ];
        assert!(tracker.take_new("hello", &words, 1_200).is_empty());

        // Now "hello world" is committed, which proves "hello" ended.
        let emitted = tracker.take_new("hello world", &words, 1_200);
        assert_eq!(texts(&emitted), ["hello"]);

        // The tentative word did get rewritten. We never pushed it, so the
        // corrected version is what goes out.
        let words = vec![
            word(" hello", 0, 500),
            word(" word", 500, 950),
            word(" again", 950, 1_200),
        ];
        let emitted = tracker.take_new("hello word again", &words, 1_200);
        assert_eq!(texts(&emitted), ["word"]);
    }

    #[test]
    fn tolerates_spacing_and_punctuation_differences() {
        let mut tracker = CommitTracker::default();
        // Committed text carries punctuation and normalized spacing that the
        // rows do not. "world" is the trailing row, so it waits.
        let words = vec![word("hello", 0, 500), word("world", 500, 900)];

        let emitted = tracker.take_new("Hmm... hello, world!", &words, 900);
        assert_eq!(texts(&emitted), ["hello,"]);
        assert_eq!(
            texts(&tracker.take_final("Hmm... hello, world!", &words, 900)),
            ["world!"]
        );
    }

    #[test]
    fn preserves_quotes_on_the_word_they_surround() {
        let mut tracker = CommitTracker::default();
        let words = vec![
            word("he", 0, 100),
            word("said", 100, 200),
            word("hello", 200, 300),
        ];

        assert_eq!(
            texts(&tracker.take_final("he said \"hello.\"", &words, 300)),
            ["he", "said", "\"hello.\""]
        );
    }

    #[test]
    fn clamps_to_the_families_drain_point() {
        let mut tracker = CommitTracker::default();
        let words = vec![word("hello", 0, 500), word("world", 500, 900)];

        // Text says both are committed, but the family has only drained 600ms
        // — and "world" is the trailing row anyway.
        let emitted = tracker.take_new("hello world", &words, 600);
        assert_eq!(texts(&emitted), ["hello"]);

        // A third row proves "world" ended, and the drain point now covers it.
        let words = vec![
            word("hello", 0, 500),
            word("world", 500, 900),
            word("again", 900, 1_200),
        ];
        let emitted = tracker.take_new("hello world again", &words, 900);
        assert_eq!(texts(&emitted), ["world"]);
    }

    #[test]
    fn ignores_the_drain_point_when_the_family_reports_none() {
        let mut tracker = CommitTracker::default();
        let words = vec![word("hello", 0, 500), word("world", 500, 900)];

        let emitted = tracker.take_new("hello world", &words, 0);
        assert_eq!(texts(&emitted), ["hello"]);
    }

    #[test]
    fn finalize_flushes_everything_left() {
        let mut tracker = CommitTracker::default();
        let words = vec![word("hello", 0, 500), word("world", 500, 900)];

        assert_eq!(
            texts(&tracker.take_new("hello world", &words, 500)),
            ["hello"]
        );
        assert_eq!(
            texts(&tracker.take_final("hello world", &words, 900)),
            ["world"]
        );
        assert!(tracker.take_final("hello world", &words, 900).is_empty());
    }

    #[test]
    fn untimed_fallback_emits_the_committed_delta() {
        let mut tracker = CommitTracker::default();

        let first = tracker.take_new("hello", &[], 500);
        assert_eq!(texts(&first), ["hello"]);
        assert_eq!((first[0].t0_ms, first[0].t1_ms), (0, 500));

        let second = tracker.take_new("hello world", &[], 900);
        assert_eq!(texts(&second), ["world"]);
        assert_eq!((second[0].t0_ms, second[0].t1_ms), (500, 900));

        assert!(tracker.take_new("hello world", &[], 900).is_empty());
    }

    #[test]
    fn handles_multibyte_text() {
        let mut tracker = CommitTracker::default();
        let words = vec![word("dörrhandtaget", 0, 500), word("på", 500, 700)];

        let emitted = tracker.take_final("dörrhandtaget på", &words, 700);
        assert_eq!(texts(&emitted), ["dörrhandtaget", "på"]);
    }

    #[test]
    fn skips_empty_word_rows() {
        let mut tracker = CommitTracker::default();
        let words = vec![
            word(" ", 0, 10),
            word("hello", 10, 500),
            word("there", 500, 800),
        ];

        let emitted = tracker.take_new("hello there", &words, 800);
        assert_eq!(texts(&emitted), ["hello"]);
    }
}
