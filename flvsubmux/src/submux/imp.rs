// SPDX-License-Identifier: MPL-2.0

//! Strict, GAP-driven FLV subtitle aggregator.

use std::sync::Mutex;

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst_base::prelude::*;
use gst_base::subclass::prelude::*;

use crate::amf::MessageName;
use crate::caption::{CaptionError, CaptionTimeline, InputMode as CaptionInputMode};
use crate::flv::MAXIMUM_TIMESTAMP_MS;

static CAT: std::sync::LazyLock<gst::DebugCategory> = std::sync::LazyLock::new(|| {
  gst::DebugCategory::new(
    "flvsubmux",
    gst::DebugColorFlags::empty(),
    Some("strict GAP-driven FLV subtitle aggregation"),
  )
});

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, glib::Enum)]
#[enum_type(name = "GstFlvSubMuxInputMode")]
pub enum InputMode {
  #[default]
  #[enum_value(name = "Timed", nick = "timed")]
  Timed,
  #[enum_value(name = "Replacement", nick = "replacement")]
  Replacement,
}

impl From<InputMode> for CaptionInputMode {
  fn from(value: InputMode) -> Self {
    match value {
      InputMode::Timed => Self::Timed,
      InputMode::Replacement => Self::Replacement,
    }
  }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, glib::Enum)]
#[enum_type(name = "GstFlvSubMuxMessageName")]
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
  input_mode: InputMode,
  prime: bool,
}

impl Default for Settings {
  fn default() -> Self {
    Self {
      message_name: MessageNameProperty::default(),
      input_mode: InputMode::default(),
      prime: true,
    }
  }
}

#[derive(Clone, Copy, Debug)]
struct TextInterval {
  start: gst::ClockTime,
  end: gst::ClockTime,
}

#[derive(Default)]
struct State {
  origin: Option<gst::ClockTime>,
  stream_position_ms: u64,
  last_media_running_time: Option<gst::ClockTime>,
  current_text: Option<TextInterval>,
  consumed_text_end: Option<gst::ClockTime>,
  caption: CaptionTimeline,
  primed: bool,
  /// A cue was on screen when the text timeline restarted, so downstream is
  /// still displaying text that no longer belongs to the current timeline.
  needs_timeline_clear: bool,
}

impl State {
  fn reset_runtime(&mut self) {
    *self = Self::default();
  }
}

pub struct FlvSubMux {
  flv_sink: gst_base::AggregatorPad,
  text_sink: gst_base::AggregatorPad,
  settings: Mutex<Settings>,
  state: Mutex<State>,
}

impl FlvSubMux {
  fn need_data() -> Result<gst::FlowSuccess, gst::FlowError> {
    Err(gst_base::AGGREGATOR_FLOW_NEED_DATA)
  }

  fn protocol_error(&self, message: impl std::fmt::Display) -> gst::FlowError {
    gst::error!(CAT, imp = self, "{message}");
    self.post_error_message(gst::error_msg!(gst::CoreError::Failed, ["{message}"]));
    gst::FlowError::Error
  }

  fn interval_error(&self, error: CaptionError) -> gst::FlowError {
    self.protocol_error(error.to_string())
  }

  /// Convert a pad timestamp to running time under the strict contract.
  ///
  /// `GstAggregatorPad` maintains its own segment, updated from `sink_event`
  /// in stream order, so reading it here always yields the segment that owns
  /// the buffer being processed.
  ///
  /// Three cases are refused rather than approximated, because each one would
  /// otherwise produce a plausible but wrong tag timestamp:
  ///
  /// * a non-TIME segment, which the base class already treats as fatal for
  ///   GAP events;
  /// * a segment whose rate is not 1.0, because running time is scaled by
  ///   `1 / |rate|` and a reverse rate additionally inverts intervals, and
  ///   neither trick-mode nor reverse playback has any meaning for a live FLV
  ///   mux;
  /// * a timestamp outside the segment, which is genuine information about
  ///   the stream rather than something to paper over.
  fn running_time(
    &self,
    pad: &gst_base::AggregatorPad,
    pts: gst::ClockTime,
  ) -> Result<gst::ClockTime, gst::FlowError> {
    let segment = match pad.segment().downcast::<gst::ClockTime>() {
      Ok(segment) => segment,
      Err(segment) => {
        return Err(self.protocol_error(format_args!(
          "pad {} has a {:?} segment; only TIME segments are supported",
          pad.name(),
          segment.format()
        )));
      }
    };

    if segment.rate() != 1.0 {
      return Err(self.protocol_error(format_args!(
        "pad {} has segment rate {}; only rate 1.0 is supported",
        pad.name(),
        segment.rate()
      )));
    }

    segment.to_running_time(pts).ok_or_else(|| {
      self.protocol_error(format_args!(
        "timestamp {pts} on pad {} lies outside its segment",
        pad.name()
      ))
    })
  }

  fn load_text_interval(&self, state: &mut State) -> Result<Option<TextInterval>, gst::FlowError> {
    if let Some(interval) = state.current_text {
      return Ok(Some(interval));
    }

    // No queued interval. The caller distinguishes "waiting for more text"
    // from "text is finished" by checking pad EOS itself.
    let Some(buffer) = self.text_sink.peek_buffer() else {
      return Ok(None);
    };

    let pts = buffer
      .pts()
      .ok_or_else(|| self.protocol_error("text buffer requires PTS"))?;
    let duration = buffer
      .duration()
      .ok_or_else(|| self.protocol_error("text buffer requires duration"))?;
    let start = self.running_time(&self.text_sink, pts)?;
    // Convert the end through the segment as well. Buffer duration is
    // expressed in segment time, so adding it to a running time is only valid
    // at rate 1.0 -- which `running_time` has just enforced, but converting
    // both edges keeps the interval correct by construction rather than by
    // coincidence.
    let end = self.running_time(&self.text_sink, pts + duration)?;
    let gap = buffer.flags().contains(gst::BufferFlags::GAP);

    // Validate placement against the already-consumed timeline *before*
    // queueing any transition, so a rejected interval cannot leave a
    // half-applied cue behind in the caption state machine.
    if let Some(previous_end) = state.consumed_text_end {
      if start < previous_end {
        return Err(self.protocol_error(format_args!(
          "text interval starts at {start}, inside already-consumed text timeline ending at {previous_end}"
        )));
      }
      if start > previous_end {
        return Err(self.protocol_error(format_args!(
          "text timeline has an uncovered interval from {previous_end} to {start}"
        )));
      }
    }

    if gap {
      if duration.is_zero() {
        return Err(self.protocol_error("GAP buffer requires a non-zero duration"));
      }
    } else {
      let (_, _, text) =
        CaptionTimeline::decode_text_buffer(&buffer).map_err(|error| self.interval_error(error))?;
      if crate::caption::warn_if_amf_text_is_long(&text) {
        gst::warning!(
          CAT,
          imp = self,
          "cue text of {} bytes exceeds the AMF0 string limit and will be truncated",
          text.len()
        );
      }
      let mode: CaptionInputMode = self.settings.lock().unwrap().input_mode.into();
      state
        .caption
        .queue_text(start, duration, text, mode)
        .map_err(|error| self.interval_error(error))?;
    }

    let interval = TextInterval { start, end };
    state.current_text = Some(interval);
    Ok(Some(interval))
  }

  fn consume_text_interval(&self, state: &mut State) -> Result<(), gst::FlowError> {
    let Some(interval) = state.current_text.take() else {
      return Ok(());
    };
    if self.text_sink.pop_buffer().is_none() {
      return Err(self.protocol_error("text interval disappeared from aggregator queue"));
    }
    state.consumed_text_end = Some(interval.end);
    Ok(())
  }

  /// Ensure that `media_time` is covered by the current text/GAP interval.
  ///
  /// The current interval remains queued while media advances through it.
  /// This is the key difference from consuming one text buffer per aggregate
  /// call: a single GAP can watermark any number of FLV tags.
  fn require_text_coverage(
    &self,
    state: &mut State,
    media_time: gst::ClockTime,
  ) -> Result<bool, gst::FlowError> {
    loop {
      let Some(interval) = self.load_text_interval(state)? else {
        if self.text_sink.is_eos() {
          return Ok(false);
        }
        return Err(gst_base::AGGREGATOR_FLOW_NEED_DATA);
      };

      if media_time < interval.start {
        return Err(self.protocol_error(format_args!(
          "media at {media_time} has no proven initial text coverage; next interval starts at {}",
          interval.start
        )));
      }

      if media_time < interval.end {
        return Ok(true);
      }

      self.consume_text_interval(state)?;
      if !self.text_sink.has_buffer() && !self.text_sink.is_eos() {
        return Err(gst_base::AGGREGATOR_FLOW_NEED_DATA);
      }
    }
  }

  /// Push output downstream.
  ///
  /// This blocks for as long as downstream takes, so it must never be called
  /// while `state` is locked: the event path needs that lock to stay
  /// responsive to flushes and to further text coverage while media is
  /// backpressured.
  fn output_list(
    &self,
    buffers: impl IntoIterator<Item = gst::Buffer>,
  ) -> Result<gst::FlowSuccess, gst::FlowError> {
    let list: gst::BufferList = buffers.into_iter().collect();
    if list.is_empty() {
      return Ok(gst::FlowSuccess::Ok);
    }
    let obj = self.obj();
    obj
      .upcast_ref::<gst_base::Aggregator>()
      .finish_buffer_list(list)
  }

  /// Decide what this FLV buffer should emit, mutating only `state`.
  ///
  /// Deliberately returns the output instead of pushing it, so the caller can
  /// release the state lock before touching downstream.
  fn process_flv_buffer(&self, state: &mut State) -> Result<Vec<gst::Buffer>, gst::FlowError> {
    let buffer = self
      .flv_sink
      .peek_buffer()
      .ok_or_else(|| self.protocol_error("FLV buffer disappeared from aggregator queue"))?;
    let Some(pts) = buffer.pts() else {
      let buffer = self
        .flv_sink
        .pop_buffer()
        .ok_or_else(|| self.protocol_error("FLV buffer disappeared from aggregator queue"))?;
      return Ok(vec![buffer]);
    };

    let running_time = self.running_time(&self.flv_sink, pts)?;
    if let Some(previous) = state.last_media_running_time {
      if running_time < previous {
        return Err(self.protocol_error(format_args!(
          "FLV timestamps are not monotonic: {running_time} follows {previous}"
        )));
      }
    }
    self.require_text_coverage(state, running_time)?;

    let buffer = self
      .flv_sink
      .pop_buffer()
      .ok_or_else(|| self.protocol_error("FLV buffer disappeared from aggregator queue"))?;

    let origin = *state.origin.get_or_insert_with(|| {
      gst::debug!(CAT, imp = self, "calibrated cue origin at {running_time}");
      running_time
    });
    state.stream_position_ms = running_time
      .saturating_sub(origin)
      .mseconds()
      .min(MAXIMUM_TIMESTAMP_MS);
    state.last_media_running_time = Some(running_time);

    let settings = *self.settings.lock().unwrap();
    let mut output = Vec::new();
    if settings.prime && !state.primed {
      let body = crate::amf::script_data_body(settings.message_name.into(), "", None);
      output.push(gst::Buffer::from_mut_slice(crate::flv::script_data_tag(
        u32::try_from(state.stream_position_ms).unwrap_or(u32::MAX),
        &body,
      )));
      state.primed = true;
    }

    // A text-timeline discontinuity left a cue on screen. Erase it at the
    // current media position before any cue from the new timeline is written.
    if state.needs_timeline_clear {
      let body = crate::amf::script_data_body(settings.message_name.into(), "", None);
      output.push(gst::Buffer::from_mut_slice(crate::flv::script_data_tag(
        u32::try_from(state.stream_position_ms).unwrap_or(u32::MAX),
        &body,
      )));
      state.needs_timeline_clear = false;
      gst::debug!(
        CAT,
        imp = self,
        "cleared stale cue at {}ms after a text timeline restart",
        state.stream_position_ms
      );
    }

    let tags = state
      .caption
      .drain_ready_ms(
        origin,
        state.stream_position_ms,
        settings.message_name.into(),
      )
      .map_err(|error| self.interval_error(error))?;
    output.extend(tags);
    output.push(buffer);
    Ok(output)
  }

  /// Drain the final caption transitions once media has ended.
  ///
  /// Like `process_flv_buffer`, this only computes output; the caller pushes
  /// it after dropping the state lock.
  fn finish_after_flv_eos(&self, state: &mut State) -> Result<Vec<gst::Buffer>, gst::FlowError> {
    // Consume all text buffers already queued after FLV EOS. This also lets
    // the aggregator reach a text EOS event that is serialized behind those
    // buffers; refusing to pop them would leave that event stranded forever.
    // No media remains to be covered, so the final interval can be consumed
    // for validation even if the text EOS event has not been processed yet.
    loop {
      if state.current_text.is_none() && self.load_text_interval(state)?.is_none() {
        break;
      }
      self.consume_text_interval(state)?;
    }

    // FLV EOS is not sufficient. Wait for the text EOS event so all final
    // transcription buffers have arrived and no future interval can appear.
    if !self.text_sink.is_eos() {
      return Err(gst_base::AGGREGATOR_FLOW_NEED_DATA);
    }

    let settings = *self.settings.lock().unwrap();
    let mut tags = Vec::new();
    if let Some(origin) = state.origin {
      tags = state
        .caption
        .drain_ready_ms(
          origin,
          state.stream_position_ms,
          settings.message_name.into(),
        )
        .map_err(|error| self.interval_error(error))?;
    }
    let discarded = state.caption.discard_pending();
    if discarded > 0 {
      gst::debug!(
        CAT,
        imp = self,
        "discarded {discarded} transitions beyond final media"
      );
    }
    Ok(tags)
  }
}

#[glib::object_subclass]
impl ObjectSubclass for FlvSubMux {
  const NAME: &'static str = "GstFlvSubMux";
  type Type = super::FlvSubMux;
  type ParentType = gst_base::Aggregator;

  fn with_class(class: &Self::Class) -> Self {
    let make_pad = |name: &str| {
      gst::Pad::builder_from_template(&class.pad_template(name).unwrap())
        .build()
        .downcast::<gst_base::AggregatorPad>()
        .expect("flvsubmux pad template must create AggregatorPad")
    };
    Self {
      flv_sink: make_pad("sink"),
      text_sink: make_pad("text"),
      settings: Mutex::new(Settings::default()),
      state: Mutex::new(State::default()),
    }
  }
}

impl ObjectImpl for FlvSubMux {
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
        glib::ParamSpecEnum::builder::<InputMode>("input-mode")
          .nick("Input mode")
          .blurb("Interpret text buffers as timed intervals or persistent replacement states")
          .default_value(InputMode::default())
          .mutable_ready()
          .build(),
        glib::ParamSpecBoolean::builder("prime")
          .nick("Prime subtitle stream")
          .blurb("Write one empty cue at the first timestamped FLV position")
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
      "input-mode" => settings.input_mode = value.get().expect("input-mode"),
      "prime" => settings.prime = value.get().expect("prime"),
      other => unimplemented!("set {other}"),
    }
  }

  fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
    let settings = self.settings.lock().unwrap();
    match pspec.name() {
      "message-name" => settings.message_name.to_value(),
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
  }
}

impl GstObjectImpl for FlvSubMux {}

impl ElementImpl for FlvSubMux {
  fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
    static METADATA: std::sync::OnceLock<gst::subclass::ElementMetadata> =
      std::sync::OnceLock::new();
    Some(METADATA.get_or_init(|| {
      gst::subclass::ElementMetadata::new(
        "Strict FLV subtitle muxer",
        "Muxer/Subtitle",
        "Aggregates video/x-flv with a continuous UTF-8 text/GAP timeline",
        "Elliott Linder <elliott@linder.dev>",
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
      let pad_type = gst_base::AggregatorPad::static_type();
      vec![
        gst::PadTemplate::builder(
          "sink",
          gst::PadDirection::Sink,
          gst::PadPresence::Always,
          &flv_caps,
        )
        .gtype(pad_type)
        .build()
        .unwrap(),
        gst::PadTemplate::builder(
          "text",
          gst::PadDirection::Sink,
          gst::PadPresence::Always,
          &text_caps,
        )
        .gtype(pad_type)
        .build()
        .unwrap(),
        gst::PadTemplate::builder(
          "src",
          gst::PadDirection::Src,
          gst::PadPresence::Always,
          &flv_caps,
        )
        .gtype(pad_type)
        .build()
        .unwrap(),
      ]
    })
  }

  fn change_state(
    &self,
    transition: gst::StateChange,
  ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
    if transition == gst::StateChange::ReadyToPaused {
      self.state.lock().unwrap().reset_runtime();
    }
    self.parent_change_state(transition)
  }
}

impl AggregatorImpl for FlvSubMux {
  fn aggregate(&self, _timeout: bool) -> Result<gst::FlowSuccess, gst::FlowError> {
    let media_ended = self.flv_sink.peek_buffer().is_none();
    if media_ended && !self.flv_sink.is_eos() {
      return Self::need_data();
    }

    // Compute output under the lock, then release it before pushing. Holding
    // `state` across `finish_buffer_list` would block every event on both
    // sink pads for the duration of a downstream push, which would defeat the
    // per-pad queueing this element relies on.
    let output = {
      let mut state = self.state.lock().unwrap();
      if media_ended {
        self.finish_after_flv_eos(&mut state)?
      } else {
        self.process_flv_buffer(&mut state)?
      }
    };

    if !output.is_empty() {
      self.output_list(output)?;
    }

    if media_ended {
      return Err(gst::FlowError::Eos);
    }
    Ok(gst::FlowSuccess::Ok)
  }

  fn flush(&self) -> Result<gst::FlowSuccess, gst::FlowError> {
    let result = self.parent_flush();
    self.state.lock().unwrap().reset_runtime();
    result
  }

  /// Handle events in stream order.
  ///
  /// `sink_event` runs on the aggregate thread once the event reaches the
  /// head of the pad queue, so a segment change applies exactly where it
  /// belongs relative to the buffers around it. Doing this in
  /// `sink_event_pre_queue` would apply it at enqueue time instead, while
  /// buffers from the previous segment were still waiting to be processed.
  fn sink_event(&self, pad: &gst_base::AggregatorPad, event: gst::Event) -> bool {
    if let gst::EventView::Segment(_) = event.view() {
      if pad == &self.flv_sink {
        // The media pad defines the output timeline: `origin`, priming and
        // every emitted tag timestamp are calibrated from it, so a new media
        // segment invalidates all of them.
        self.state.lock().unwrap().reset_runtime();
      } else {
        // A new text segment declares a new caption timeline. Coverage
        // markers, pending transitions and the active cue all describe the
        // old one, so they are discarded together rather than left to leak
        // across the discontinuity.
        //
        // If a cue was on screen, downstream is still displaying text that no
        // longer belongs to the stream. Note that, so the next media buffer
        // carries an explicit clear -- the same reason `tttocea708` erases the
        // display rather than simply stopping.
        let mut state = self.state.lock().unwrap();
        state.current_text = None;
        state.consumed_text_end = None;
        state.needs_timeline_clear = state.caption.has_active_text();
        state.caption.reset_timeline();
      }
    }
    self.parent_sink_event(pad, event)
  }
}
