// SPDX-License-Identifier: MPL-2.0

//! Splices FLV script-data subtitle tags into an already-muxed FLV stream.
//!
//! # Why this sits after the muxer
//!
//! `flvmux`/`eflvmux` declare exactly two sink pad templates, `video` and
//! `audio`, and write exactly one script-data message, `onMetaData`. There is
//! no text pad to request and no hook for arbitrary AMF messages. The muxer's
//! output, however, is a flat sequence of self-delimiting tags, each stamped
//! with a rebased millisecond timestamp — so a correctly framed script-data tag
//! can simply be spliced between two existing tags.
//!
//! Placing this after the muxer has a second consequence that matters more than
//! the convenience: the text path never enters an aggregator. `cccombiner` and
//! `matroskamux` both block until every sink pad is non-empty, which is what
//! forces keepalive machinery onto sparse caption branches. This element polls
//! nothing and waits for nothing; a silent text pad costs zero bytes and zero
//! latency.
//!
//! # What this element is *not* responsible for
//!
//! Deliberately narrow. It does not wrap text, choose caption layout, clear on
//! silence, or know that speech recognition exists. It translates either
//! timed text intervals or explicit replacement states into FLV transitions.
//!
//! In particular, a cue whose text is empty is *not* filtered out here: a
//! caller that wants to signal "clear the display" must be able to, and
//! suppressing it would silently strand the previous caption on screen.
//!
//! FLV script data has no explicit erase, so "clear the display" is serialized
//! as an empty text state. `textrollup` emits one at `clear-after`; ordinary
//! timed cues get one scheduled from their duration in `input-mode=timed`.
//!
//! Priming uses an empty state: it declares the subtitle stream while leaving
//! stateful consumers explicitly blank until the first real cue arrives.

use std::sync::Mutex;

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;

use crate::amf::{script_data_body, MessageName};
use crate::flv::{script_data_tag, MAXIMUM_TIMESTAMP_MS};

static CAT: std::sync::LazyLock<gst::DebugCategory> = std::sync::LazyLock::new(|| {
  gst::DebugCategory::new(
    "flvsubinject",
    gst::DebugColorFlags::empty(),
    Some("FLV script-data subtitle injection"),
  )
});

/// What to do with a cue whose timestamp the FLV stream has already passed.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, glib::Enum)]
#[enum_type(name = "GstFlvSubInjectLatePolicy")]
pub enum LatePolicy {
  /// Emit it at the current stream position rather than its own timestamp.
  ///
  /// A late caption displayed slightly early is legible; a dropped one is a
  /// gap in the transcript. This is the default because the cue text itself is
  /// still correct, only its placement has degraded.
  #[default]
  #[enum_value(name = "Clamp", nick = "clamp")]
  Clamp,
  /// Discard it.
  ///
  /// Appropriate when a downstream consumer has already served the time range,
  /// which is a judgement only that consumer can make.
  #[enum_value(name = "Drop", nick = "drop")]
  Drop,
}

/// How input buffer timing maps to FLV display-state transitions.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, glib::Enum)]
#[enum_type(name = "GstFlvSubInjectInputMode")]
pub enum InputMode {
  /// A non-empty buffer shows at PTS and clears at PTS + duration.
  #[default]
  #[enum_value(name = "Timed", nick = "timed")]
  Timed,
  /// Buffers are persistent replacement states; only empty text clears.
  #[enum_value(name = "Replacement", nick = "replacement")]
  Replacement,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, glib::Enum)]
#[enum_type(name = "GstFlvSubInjectMessageName")]
pub enum MessageNameProperty {
  #[default]
  #[enum_value(name = "onCaption", nick = "oncaption")]
  OnCaption,
  #[enum_value(name = "onTextData", nick = "ontextdata")]
  OnTextData,
}

impl From<MessageNameProperty> for MessageName {
  fn from(value: MessageNameProperty) -> Self {
    match value {
      MessageNameProperty::OnCaption => Self::OnCaption,
      MessageNameProperty::OnTextData => Self::OnTextData,
    }
  }
}

#[derive(Clone, Copy, Debug)]
struct Settings {
  message_name: MessageNameProperty,
  late_policy: LatePolicy,
  input_mode: InputMode,
  prime: bool,
}

impl Default for Settings {
  fn default() -> Self {
    Self {
      message_name: MessageNameProperty::default(),
      late_policy: LatePolicy::default(),
      input_mode: InputMode::default(),
      prime: true,
    }
  }
}

/// Payload of the priming cue.
///
/// Empty text is the same explicit blank state used for normal clears. This
/// still makes the subtitle stream discoverable, without activating an
/// invisible state that downstream consumers can mistake for a lost clear.
const PRIMING_TEXT: &str = "";

/// How often to repeat the one-tag-per-buffer warning once it starts firing.
///
/// The first violation is always logged; after that one line per this many
/// keeps a continuous fault visible without flooding at buffer rate.
const INVARIANT_WARNING_INTERVAL: u64 = 1_000;

#[derive(Clone, Debug)]
enum TransitionKind {
  Show { generation: u64, text: String },
  /// `Some` is a timed cue's scheduled end; `None` is an explicit clear.
  Clear { generation: Option<u64> },
}

/// A display-state transition waiting for muxed media to reach its timestamp.
#[derive(Clone, Debug)]
struct PendingTransition {
  running_time: gst::ClockTime,
  sequence: u64,
  kind: TransitionKind,
}

#[derive(Clone, Debug)]
struct ActiveState {
  generation: u64,
  text: String,
}

#[derive(Default)]
struct State {
  /// Transitions not yet applied, ordered by media time then arrival sequence.
  pending: Vec<PendingTransition>,
  /// Segments observed on each sink pad, for running-time conversion.
  ///
  /// Both timelines must be expressed in the same domain before they can be
  /// compared, and running time is the only one both branches share.
  flv_segment: Option<gst::FormattedSegment<gst::ClockTime>>,
  text_segment: Option<gst::FormattedSegment<gst::ClockTime>>,
  /// Millisecond timestamp of the most recent tag forwarded.
  stream_position_ms: u32,
  /// Running time of the first A/V tag, used to rebase cue timestamps.
  ///
  /// `flvmux` writes tag timestamps relative to its own first timestamp, while
  /// the text pad's buffers carry pipeline running time. Calibrating from the
  /// first tag observed is what keeps the two in one domain: the alternative,
  /// assuming both start at zero, silently skews whenever the muxer's first
  /// buffer does not.
  origin: Option<gst::ClockTime>,
  active: Option<ActiveState>,
  next_generation: u64,
  next_sequence: u64,
  shows: u64,
  clears: u64,
  identical_suppressed: u64,
  late_clamped: u64,
  late_dropped: u64,
  future_discarded: u64,
  /// Whether the priming cue has been written.
  primed: bool,
}

impl State {
  fn running_time(
    segment: Option<&gst::FormattedSegment<gst::ClockTime>>,
    pts: gst::ClockTime,
  ) -> gst::ClockTime {
    segment
      .and_then(|segment| segment.to_running_time(pts))
      .unwrap_or(pts)
  }
}

pub struct FlvSubInject {
  flv_sink: gst::Pad,
  text_sink: gst::Pad,
  src: gst::Pad,
  settings: Mutex<Settings>,
  state: Mutex<State>,
  /// How many buffers have violated the one-tag-per-buffer invariant.
  invariant_violations: std::sync::atomic::AtomicU64,
}

impl FlvSubInject {
  /// Accept a cue from the text pad.
  ///
  /// This runs on the *text* streaming thread, which is not the thread carrying
  /// FLV bytes. It therefore only ever queues: it must never touch the tag
  /// stream or push anything downstream.
  ///
  /// The reason is the difference between this element and `cccombiner`.
  /// `cccombiner` is a `GstAggregator`, so alignment is structural: a single
  /// `aggregate()` call pulls from both pads on one thread, and nothing it
  /// emits can interleave or misorder. This element is a plain `GstElement`
  /// with two independently-scheduled chain functions, so that guarantee has
  /// to be built rather than inherited.
  ///
  /// It is built by making the FLV thread the only writer. If this thread also
  /// pushed, two failures would follow, and the first is far worse than the
  /// second:
  ///
  /// 1. **Framing.** The FLV thread emits whole tags, but it holds a partially
  ///    consumed buffer between them. A push from here can land between two
  ///    `src.push()` calls of a single chain invocation, splicing a script tag
  ///    into the middle of another tag's body. That does not lose one caption;
  ///    it desynchronizes the byte stream and every byte after it is garbage.
  /// 2. **Ordering.** Even when framing survives, a cue written without
  ///    reference to the current tag position can land after a later-timestamped
  ///    A/V tag, so a consumer that trusts tag order mis-times it.
  fn handle_text_buffer(&self, buffer: &gst::Buffer) -> Result<gst::FlowSuccess, gst::FlowError> {
    let pts = buffer.pts().ok_or_else(|| {
      gst::error!(CAT, imp = self, "text buffer requires PTS");
      gst::FlowError::Error
    })?;
    let duration = buffer.duration().ok_or_else(|| {
      gst::error!(CAT, imp = self, "text buffer requires duration");
      gst::FlowError::Error
    })?;

    let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;
    let text = std::str::from_utf8(map.as_slice())
      .map_err(|error| {
        gst::error!(CAT, imp = self, "text buffer is not UTF-8: {error}");
        gst::FlowError::Error
      })?
      .to_owned();

    // AMF0 short strings cap at 64KB and `script_data_body` truncates to fit.
    // Silent truncation is a data-loss path, so it is surfaced here where the
    // original length is still known.
    if text.len() > crate::amf::MAXIMUM_TEXT_BYTES {
      gst::warning!(
        CAT,
        imp = self,
        "cue text of {} bytes exceeds the {}-byte AMF0 string limit and will be truncated",
        text.len(),
        crate::amf::MAXIMUM_TEXT_BYTES
      );
    }

    let input_mode = self.settings.lock().unwrap().input_mode;
    let mut state = self.state.lock().unwrap();
    let start = State::running_time(state.text_segment.as_ref(), pts);
    let generation = state.next_generation;
    state.next_generation += 1;
    let mut queue = |running_time: gst::ClockTime, kind: TransitionKind| {
      let sequence = state.next_sequence;
      state.next_sequence += 1;
      state.pending.push(PendingTransition {
        running_time,
        sequence,
        kind,
      });
    };

    match input_mode {
      InputMode::Timed => {
        if text.is_empty() {
          queue(start, TransitionKind::Clear { generation: None });
        } else {
          if duration.is_zero() {
            gst::error!(CAT, imp = self, "timed non-empty text needs non-zero duration");
            return Err(gst::FlowError::Error);
          }
          queue(
            start,
            TransitionKind::Show {
              generation,
              text,
            },
          );
          queue(
            start + duration,
            TransitionKind::Clear {
              generation: Some(generation),
            },
          );
        }
      }
      InputMode::Replacement => {
        let kind = if text.is_empty() {
          TransitionKind::Clear { generation: None }
        } else {
          TransitionKind::Show {
            generation,
            text,
          }
        };
        queue(start, kind);
      }
    }

    gst::log!(
      CAT,
      imp = self,
      "queued {input_mode:?} state at {start} for {duration}"
    );
    state
      .pending
      .sort_by_key(|transition| (transition.running_time, transition.sequence));
    Ok(gst::FlowSuccess::Ok)
  }

  /// Serialize every queued cue the stream position has reached.
  ///
  /// Only ever called from the FLV streaming thread, which is what keeps the
  /// element single-writer. A cue therefore leaves the queue when the next tag
  /// arrives rather than the instant it is produced.
  ///
  /// That is a real latency bound, and it is the muxer's cadence rather than
  /// anything this element adds: video at 30fps means a tag roughly every
  /// 33ms, and audio adds more. A caption is delayed by at most one tag
  /// interval, which is far below the delay the caption pipeline already
  /// carries to align text with A/V. Draining eagerly from the text thread
  /// would trade that bounded delay for a corrupt byte stream.
  fn drain_ready(&self, state: &mut State) -> Vec<gst::Buffer> {
    let Some(origin) = state.origin else {
      return Vec::new();
    };
    let settings = *self.settings.lock().unwrap();

    let position_ms = u64::from(state.stream_position_ms);
    let mut reached = Vec::new();
    let mut remaining = Vec::new();
    for transition in std::mem::take(&mut state.pending) {
      let transition_ms = transition
        .running_time
        .saturating_sub(origin)
        .mseconds()
        .min(MAXIMUM_TIMESTAMP_MS);
      if transition_ms > position_ms {
        remaining.push(transition);
        continue;
      }

      let timestamp_ms = if transition_ms < position_ms {
        match settings.late_policy {
          LatePolicy::Drop => {
            state.late_dropped += 1;
            gst::debug!(
              CAT,
              imp = self,
              "dropping transition {transition_ms}ms behind stream position {position_ms}ms"
            );
            continue;
          }
          LatePolicy::Clamp => {
            state.late_clamped += 1;
            position_ms
          }
        }
      } else {
        transition_ms
      };
      reached.push((timestamp_ms, transition.sequence, transition.kind));
    }
    state.pending = remaining;

    // Apply every transition at a timestamp before serializing. Adjacent
    // clear/show pairs and multiple updates reached on one media tag therefore
    // expose only their final state, never a transient blank.
    reached.sort_by_key(|(timestamp, sequence, _)| (*timestamp, *sequence));
    let mut tags = Vec::new();
    let mut cursor = 0;
    while cursor < reached.len() {
      let timestamp_ms = reached[cursor].0;
      let visible_before = state.active.as_ref().map(|active| active.text.clone());
      while cursor < reached.len() && reached[cursor].0 == timestamp_ms {
        match &reached[cursor].2 {
          TransitionKind::Show { generation, text } => {
            state.active = Some(ActiveState {
              generation: *generation,
              text: text.clone(),
            });
          }
          TransitionKind::Clear { generation } => {
            // A clear belongs to the cue generation that scheduled it. An
            // overlapping replacement has already taken ownership of display.
            if generation.is_none()
              || state
                .active
                .as_ref()
                .is_some_and(|active| Some(active.generation) == *generation)
            {
              state.active = None;
            }
          }
        }
        cursor += 1;
      }

      let visible_after = state.active.as_ref().map(|active| active.text.clone());
      if visible_before == visible_after {
        state.identical_suppressed += 1;
        continue;
      }
      let text = visible_after.unwrap_or_default();
      if text.is_empty() {
        state.clears += 1;
      } else {
        state.shows += 1;
      }
      let body = script_data_body(settings.message_name.into(), &text, None);
      let tag = script_data_tag(u32::try_from(timestamp_ms).unwrap_or(u32::MAX), &body);
      tags.push(gst::Buffer::from_mut_slice(tag));
    }
    tags
  }

  fn push_tags(&self, tags: Vec<gst::Buffer>) -> Result<gst::FlowSuccess, gst::FlowError> {
    for tag in tags {
      self.src.push(tag)?;
    }
    Ok(gst::FlowSuccess::Ok)
  }

  /// Apply reached transitions and discard transitions beyond final media.
  ///
  /// Runs on the FLV streaming thread, from the EOS handler, so it preserves
  /// the single-writer invariant: EOS arrives on the same pad and thread that
  /// pushes buffers, and no chain call can be in flight beside it.
  ///
  fn flush_pending_at_eos(&self) {
    let tags = {
      let mut state = self.state.lock().unwrap();
      if state.pending.is_empty() {
        return;
      }
      let tags = self.drain_ready(&mut state);
      let discarded = state.pending.len() as u64;
      state.future_discarded += discarded;
      state.pending.clear();
      gst::debug!(
        CAT,
        imp = self,
        "applied {} reached transition(s) and discarded {discarded} future transition(s) at EOS",
        tags.len()
      );
      tags
    };

    if let Err(error) = self.push_tags(tags) {
      gst::debug!(CAT, imp = self, "EOS cue drain failed: {error}");
    }
  }

  /// Forward FLV bytes, splicing queued cues at tag boundaries.
  /// Forward one muxed FLV buffer, emitting any cues it has moved past.
  ///
  /// `flvmux`/`eflvmux` emit exactly one whole FLV tag per buffer, stamped
  /// with the running time of the media it carries. Verified against 1.28.5
  /// for video-only and interleaved audio/video, including through a `queue`:
  /// buffer sizes match tag sizes exactly, and the muxer's own
  /// `gst_aggregator_finish_buffer` path has no way to emit a partial tag.
  ///
  /// So this element does not parse the byte stream. It reads the PTS the
  /// muxer already computed and forwards the buffer untouched. An earlier
  /// version accumulated bytes and parsed 11-byte tag headers to recover the
  /// same timestamps; that was defensive against something that does not
  /// happen, and the buffer boundaries it reconstructed were the ones it had
  /// been given.
  ///
  /// The stream-header buffers — FLV header, `onMetaData`, codec data — carry
  /// no PTS. They are forwarded as-is and establish nothing: the timeline is
  /// calibrated from the first *timestamped* buffer, since keying off the
  /// first buffer of any kind leaves the origin unset forever and silently
  /// strands every queued cue.
  fn handle_flv_buffer(&self, buffer: gst::Buffer) -> Result<gst::FlowSuccess, gst::FlowError> {
    self.check_one_tag_per_buffer(&buffer);

    let Some(pts) = buffer.pts() else {
      // A stream header. Nothing to place cues against yet.
      return self.src.push(buffer);
    };

    let (prime, message_name) = {
      let settings = self.settings.lock().unwrap();
      (settings.prime, MessageName::from(settings.message_name))
    };

    let mut state = self.state.lock().unwrap();
    let running_time = State::running_time(state.flv_segment.as_ref(), pts);
    let origin = *state.origin.get_or_insert_with(|| {
      gst::debug!(CAT, imp = self, "calibrated cue origin at {running_time}");
      running_time
    });

    // Tag timestamps are rebased against the muxer's first timestamp, so cues
    // have to be expressed in the same rebased domain to line up with them.
    state.stream_position_ms = u32::try_from(
      running_time
        .saturating_sub(origin)
        .mseconds()
        .min(MAXIMUM_TIMESTAMP_MS),
    )
    .unwrap_or(u32::MAX);

    let mut output: Vec<gst::Buffer> = Vec::new();

    // Declare the subtitle timeline at the very start of the stream.
    //
    // FLV has no track table: a script-data subtitle stream exists only once
    // its first cue has been seen. A demuxer that finishes probing before then
    // concludes the stream has no captions and never revisits it — FFmpeg's
    // `flv_data_packet` creates the `AV_CODEC_ID_TEXT` stream lazily, and a
    // packager built on it reports zero subtitle tracks.
    //
    // Speech-derived captions always lose that race: the first cue cannot
    // appear until someone has spoken and the recognizer has committed a word,
    // which is seconds after the probe window closes.
    //
    // One invisible cue at the head of the stream fixes it, and is the exact
    // analogue of what the carriage this replaces does: CEA-708 declares its
    // service by sending null padding from the first frame, long before any
    // caption text exists.
    if prime && !state.primed {
      let body = script_data_body(message_name, PRIMING_TEXT, None);
      output.push(gst::Buffer::from_mut_slice(script_data_tag(
        state.stream_position_ms,
        &body,
      )));
      state.primed = true;
      gst::debug!(
        CAT,
        imp = self,
        "primed subtitle stream at {}ms",
        state.stream_position_ms
      );
    }

    // Cues belonging before this tag go out ahead of it, so the output stays
    // ordered by timestamp.
    output.extend(self.drain_ready(&mut state));
    drop(state);

    output.push(buffer);
    self.push_tags(output)
  }

  /// Warn once if a buffer is not exactly one whole FLV tag.
  ///
  /// The element depends on this invariant, so it is asserted rather than
  /// assumed. It is checked instead of handled: a splitter or repackager that
  /// broke it would need a design change here, not a silent fallback, and a
  /// silent fallback is what would make the resulting mistimed captions hard
  /// to trace back to their cause.
  fn check_one_tag_per_buffer(&self, buffer: &gst::Buffer) {
    // Stream headers are concatenated small blocks rather than single tags,
    // and carry no PTS; they are forwarded untouched and are not covered.
    if buffer.pts().is_none() {
      return;
    }
    let Ok(map) = buffer.map_readable() else {
      return;
    };
    let matches_one_tag = crate::flv::parse_tag_header(map.as_slice())
      .is_some_and(|header| header.total_len() == map.size());
    if matches_one_tag {
      return;
    }

    // Throttled rather than latched. Logging every violation would flood at
    // buffer rate, but logging only the first hides whether the problem was a
    // single malformed buffer or is continuous — and that distinction is the
    // whole diagnostic value when captions come out misplaced.
    let seen = self
      .invariant_violations
      .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
      + 1;
    if seen == 1 || seen % INVARIANT_WARNING_INTERVAL == 0 {
      gst::warning!(
        CAT,
        imp = self,
        "buffer of {} bytes is not exactly one FLV tag ({seen} so far); cue \
         placement assumes one tag per buffer and may be misaligned",
        map.size()
      );
    }
  }
}

#[glib::object_subclass]
impl ObjectSubclass for FlvSubInject {
  const NAME: &'static str = "GstFlvSubInject";
  type Type = super::FlvSubInject;
  type ParentType = gst::Element;

  fn with_class(class: &Self::Class) -> Self {
    let flv_sink = gst::Pad::builder_from_template(&class.pad_template("sink").unwrap())
      .chain_function(|pad, parent, buffer| {
        FlvSubInject::catch_panic_pad_function(
          parent,
          || Err(gst::FlowError::Error),
          |this| this.handle_flv_buffer(buffer),
        )
        .inspect_err(|error| gst::debug!(CAT, obj = pad, "flv chain failed: {error}"))
      })
      .event_function(|pad, parent, event| {
        FlvSubInject::catch_panic_pad_function(
          parent,
          || false,
          |this| {
            use gst::EventView;
            match event.view() {
              EventView::Segment(segment) => {
                if let Some(segment) = segment.segment().downcast_ref::<gst::ClockTime>() {
                  this.state.lock().unwrap().flv_segment = Some(segment.clone());
                }
              }
              EventView::Eos(_) => {
                // Cues still queued belong to media that was published, so they
                // are written before the stream ends rather than discarded with
                // the element.
                //
                // The text and FLV branches run on independent threads, so the
                // transcriber's final flush can land after the muxer has already
                // finished. Dropping here loses the last caption of every
                // session, and does it invisibly. `tttocea708` handles the same
                // case by erasing the display at EOS.
                this.flush_pending_at_eos();
              }
              EventView::FlushStop(_) => {
                // A flush discards the timeline both sides were measured
                // against. Keeping `origin` or `stream_position_ms` across one
                // would place every subsequent cue against a stale reference,
                // and keeping `pending` would emit cues for media that is no
                // longer being sent.
                //
                // Settings are deliberately not reset: they are configuration,
                // not stream state.
                let mut state = this.state.lock().unwrap();
                let dropped = state.pending.len();
                *state = State::default();
                if dropped > 0 {
                  gst::debug!(CAT, imp = this, "flush discarded {dropped} queued cue(s)");
                }
              }
              _ => {}
            }
            gst::Pad::event_default(pad, Some(&*this.obj()), event)
          },
        )
      })
      .build();

    let text_sink = gst::Pad::builder_from_template(&class.pad_template("text").unwrap())
      .chain_function(|_pad, parent, buffer| {
        FlvSubInject::catch_panic_pad_function(
          parent,
          || Err(gst::FlowError::Error),
          |this| this.handle_text_buffer(&buffer),
        )
      })
      .event_function(|_pad, parent, event| {
        FlvSubInject::catch_panic_pad_function(
          parent,
          || false,
          |this| {
            // The text pad is sparse and its stream events must not reach the
            // source pad: the output is FLV, and forwarding a text caps or EOS
            // event would either renegotiate the src pad or end the stream
            // while A/V is still flowing.
            use gst::EventView;
            match event.view() {
              EventView::Segment(segment) => {
                if let Some(segment) = segment.segment().downcast_ref::<gst::ClockTime>() {
                  this.state.lock().unwrap().text_segment = Some(segment.clone());
                }
                true
              }
              EventView::Caps(_) | EventView::StreamStart(_) => true,
              EventView::Eos(_) => {
                gst::debug!(CAT, imp = this, "text stream ended; FLV continues");
                true
              }
              EventView::Gap(_) => true,
              _ => true,
            }
          },
        )
      })
      .build();

    let src = gst::Pad::builder_from_template(&class.pad_template("src").unwrap()).build();

    Self {
      flv_sink,
      text_sink,
      src,
      settings: Mutex::new(Settings::default()),
      state: Mutex::new(State::default()),
      invariant_violations: std::sync::atomic::AtomicU64::new(0),
    }
  }
}

impl ObjectImpl for FlvSubInject {
  fn properties() -> &'static [glib::ParamSpec] {
    static PROPERTIES: std::sync::OnceLock<Vec<glib::ParamSpec>> = std::sync::OnceLock::new();
    PROPERTIES.get_or_init(|| {
      vec![
        glib::ParamSpecEnum::builder::<MessageNameProperty>("message-name")
          .nick("Message name")
          .blurb("AMF0 script-data message name carrying each cue")
          .default_value(MessageNameProperty::default())
          .mutable_ready()
          .build(),
        glib::ParamSpecEnum::builder::<LatePolicy>("late-policy")
          .nick("Late cue policy")
          .blurb("What to do with a cue the FLV stream has already passed")
          .default_value(LatePolicy::default())
          .mutable_playing()
          .build(),
        glib::ParamSpecEnum::builder::<InputMode>("input-mode")
          .nick("Input mode")
          .blurb("Interpret text buffers as timed intervals or persistent replacement states")
          .default_value(InputMode::default())
          .mutable_ready()
          .build(),
        glib::ParamSpecBoolean::builder("prime")
          .nick("Prime subtitle stream")
          .blurb(
            "Write one empty cue at the start so demuxers discover the \
             subtitle stream before any caption text exists",
          )
          .default_value(true)
          .mutable_ready()
          .build(),
      ]
    })
  }

  fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
    let mut settings = self.settings.lock().unwrap();
    match pspec.name() {
      "message-name" => settings.message_name = value.get().expect("message-name"),
      "late-policy" => settings.late_policy = value.get().expect("late-policy"),
      "input-mode" => settings.input_mode = value.get().expect("input-mode"),
      "prime" => settings.prime = value.get().expect("prime"),
      other => unimplemented!("set {other}"),
    }
  }

  fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
    let settings = self.settings.lock().unwrap();
    match pspec.name() {
      "message-name" => settings.message_name.to_value(),
      "late-policy" => settings.late_policy.to_value(),
      "input-mode" => settings.input_mode.to_value(),
      "prime" => settings.prime.to_value(),
      other => unimplemented!("get {other}"),
    }
  }

  fn constructed(&self) {
    self.parent_constructed();
    let obj = self.obj();
    obj.add_pad(&self.flv_sink).unwrap();
    obj.add_pad(&self.text_sink).unwrap();
    obj.add_pad(&self.src).unwrap();
  }
}

impl GstObjectImpl for FlvSubInject {}

impl ElementImpl for FlvSubInject {
  fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
    static METADATA: std::sync::OnceLock<gst::subclass::ElementMetadata> =
      std::sync::OnceLock::new();
    Some(METADATA.get_or_init(|| {
      gst::subclass::ElementMetadata::new(
        "FLV subtitle injector",
        "Muxer/Subtitle",
        "Injects timed text into a muxed FLV stream as onCaption/onTextData script data",
        "Elliott Darfink <elliott.darfink@gmail.com>",
      )
    }))
  }

  fn pad_templates() -> &'static [gst::PadTemplate] {
    static TEMPLATES: std::sync::OnceLock<Vec<gst::PadTemplate>> = std::sync::OnceLock::new();
    TEMPLATES.get_or_init(|| {
      let flv_caps = gst::Caps::builder("video/x-flv").build();
      let text_caps = gst::Caps::builder("text/x-raw")
        .field("format", "utf8")
        .build();
      vec![
        gst::PadTemplate::new(
          "sink",
          gst::PadDirection::Sink,
          gst::PadPresence::Always,
          &flv_caps,
        )
        .unwrap(),
        gst::PadTemplate::new(
          "text",
          gst::PadDirection::Sink,
          gst::PadPresence::Always,
          &text_caps,
        )
        .unwrap(),
        gst::PadTemplate::new(
          "src",
          gst::PadDirection::Src,
          gst::PadPresence::Always,
          &flv_caps,
        )
        .unwrap(),
      ]
    })
  }

  fn change_state(
    &self,
    transition: gst::StateChange,
  ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
    if transition == gst::StateChange::ReadyToPaused {
      *self.state.lock().unwrap() = State::default();
    }
    let result = self.parent_change_state(transition)?;
    if transition == gst::StateChange::PausedToReady {
      let state = self.state.lock().unwrap();
      gst::debug!(
        CAT,
        imp = self,
        "shows={}, clears={}, identical-suppressed={}, late-clamped={}, late-dropped={}, future-discarded={}, pending={}",
        state.shows,
        state.clears,
        state.identical_suppressed,
        state.late_clamped,
        state.late_dropped,
        state.future_discarded,
        state.pending.len()
      );
    }
    Ok(result)
  }
}
