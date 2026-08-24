// SPDX-License-Identifier: MPL-2.0

//! Caption state and AMF transition scheduling for `flvsubmux`.

use std::fmt;

use crate::amf::{MAXIMUM_TEXT_BYTES, MessageName, script_data_body};
use crate::flv::{MAXIMUM_TIMESTAMP_MS, script_data_tag};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InputMode {
  Timed,
  Replacement,
}

#[derive(Debug)]
pub enum CaptionError {
  MissingPts,
  MissingDuration,
  BufferMap,
  InvalidUtf8(std::str::Utf8Error),
  ZeroDuration,
}

impl fmt::Display for CaptionError {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::MissingPts => f.write_str("text buffer requires PTS"),
      Self::MissingDuration => f.write_str("text buffer requires duration"),
      Self::BufferMap => f.write_str("text buffer could not be mapped"),
      Self::InvalidUtf8(error) => write!(f, "text buffer is not UTF-8: {error}"),
      Self::ZeroDuration => f.write_str("timed non-empty text needs non-zero duration"),
    }
  }
}

#[derive(Clone, Debug)]
enum TransitionKind {
  Show { generation: u64, text: String },
  Clear { generation: Option<u64> },
}

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
pub struct CaptionTimeline {
  pending: Vec<PendingTransition>,
  active: Option<ActiveState>,
  next_generation: u64,
  next_sequence: u64,
}

impl CaptionTimeline {
  /// Decode a normal text buffer. GAP buffers are deliberately handled by
  /// `flvsubmux` before this method and never enter the UTF-8 decoder.
  pub fn decode_text_buffer(
    buffer: &gst::Buffer,
  ) -> Result<(gst::ClockTime, gst::ClockTime, String), CaptionError> {
    let pts = buffer.pts().ok_or(CaptionError::MissingPts)?;
    let duration = buffer.duration().ok_or(CaptionError::MissingDuration)?;
    let map = buffer.map_readable().map_err(|_| CaptionError::BufferMap)?;
    let text = std::str::from_utf8(map.as_slice())
      .map_err(CaptionError::InvalidUtf8)?
      .to_owned();
    Ok((pts, duration, text))
  }

  pub fn queue_text(
    &mut self,
    start: gst::ClockTime,
    duration: gst::ClockTime,
    text: String,
    input_mode: InputMode,
  ) -> Result<(), CaptionError> {
    let generation = self.next_generation;
    self.next_generation += 1;

    let mut queue = |running_time: gst::ClockTime, kind: TransitionKind| {
      let sequence = self.next_sequence;
      self.next_sequence += 1;
      self.pending.push(PendingTransition {
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
            return Err(CaptionError::ZeroDuration);
          }
          queue(start, TransitionKind::Show { generation, text });
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
          TransitionKind::Show { generation, text }
        };
        queue(start, kind);
      }
    }

    self
      .pending
      .sort_by_key(|transition| (transition.running_time, transition.sequence));
    Ok(())
  }

  /// Serialize transitions reached by a media timestamp.
  pub fn drain_ready_ms(
    &mut self,
    origin: gst::ClockTime,
    position_ms: u64,
    message_name: MessageName,
  ) -> Result<Vec<gst::Buffer>, CaptionError> {
    let mut reached = Vec::new();
    let mut remaining = Vec::new();
    for transition in std::mem::take(&mut self.pending) {
      let transition_ms = transition
        .running_time
        .saturating_sub(origin)
        .mseconds()
        .min(MAXIMUM_TIMESTAMP_MS);
      if transition_ms > position_ms {
        remaining.push(transition);
        continue;
      }

      let timestamp_ms = transition_ms;
      reached.push((timestamp_ms, transition.sequence, transition.kind));
    }
    self.pending = remaining;

    reached.sort_by_key(|(timestamp, sequence, _)| (*timestamp, *sequence));
    let mut tags = Vec::new();
    let mut cursor = 0;
    while cursor < reached.len() {
      let timestamp_ms = reached[cursor].0;
      let visible_before = self.active.as_ref().map(|active| active.text.clone());
      while cursor < reached.len() && reached[cursor].0 == timestamp_ms {
        match &reached[cursor].2 {
          TransitionKind::Show { generation, text } => {
            self.active = Some(ActiveState {
              generation: *generation,
              text: text.clone(),
            });
          }
          TransitionKind::Clear { generation } => {
            if generation.is_none()
              || self
                .active
                .as_ref()
                .is_some_and(|active| Some(active.generation) == *generation)
            {
              self.active = None;
            }
          }
        }
        cursor += 1;
      }

      let visible_after = self.active.as_ref().map(|active| active.text.clone());
      if visible_before == visible_after {
        continue;
      }

      let text = visible_after.unwrap_or_default();
      let body = script_data_body(message_name, &text, None);
      tags.push(gst::Buffer::from_mut_slice(script_data_tag(
        u32::try_from(timestamp_ms).unwrap_or(u32::MAX),
        &body,
      )));
    }

    Ok(tags)
  }

  pub fn discard_pending(&mut self) -> u64 {
    let discarded = self.pending.len() as u64;
    self.pending.clear();
    discarded
  }

  /// Whether a cue is currently on screen downstream.
  pub fn has_active_text(&self) -> bool {
    self
      .active
      .as_ref()
      .is_some_and(|active| !active.text.is_empty())
  }

  /// Drop the whole timeline, keeping the generation counters monotonic.
  ///
  /// Used when a new segment declares a fresh text timeline: pending
  /// transitions and the active cue both belong to the old one. Counters are
  /// deliberately not rewound, so a stale `Clear` can never match a cue
  /// queued after the reset.
  pub fn reset_timeline(&mut self) {
    self.pending.clear();
    self.active = None;
  }
}

pub fn warn_if_amf_text_is_long(text: &str) -> bool {
  text.len() > MAXIMUM_TEXT_BYTES
}
