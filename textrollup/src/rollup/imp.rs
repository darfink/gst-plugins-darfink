// SPDX-License-Identifier: MPL-2.0

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;

use std::sync::{LazyLock, Mutex};

use super::window::{Push, Window};

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "textrollup",
        gst::DebugColorFlags::empty(),
        Some("Roll-up caption window"),
    )
});

const DEFAULT_COLUMNS: u32 = 42;
const DEFAULT_LINES: u32 = 2;
const DEFAULT_CLEAR_AFTER_MS: u32 = 3000;
const DEFAULT_BREAK_ON_SENTENCE: bool = true;

#[derive(Debug, Clone)]
struct Settings {
    columns: u32,
    lines: u32,
    clear_after_ms: u32,
    break_on_sentence: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            columns: DEFAULT_COLUMNS,
            lines: DEFAULT_LINES,
            clear_after_ms: DEFAULT_CLEAR_AFTER_MS,
            break_on_sentence: DEFAULT_BREAK_ON_SENTENCE,
        }
    }
}

struct State {
    window: Window,
    /// End of the most recent non-empty input buffer. Clear timing is derived
    /// from this media timestamp, never from buffer arrival or wall-clock time.
    last_input_end: Option<gst::ClockTime>,
    /// Furthest GAP frontier seen in this segment, used to reject regressions.
    frontier: Option<gst::ClockTime>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            window: Window::new(
                DEFAULT_COLUMNS as usize,
                DEFAULT_LINES as usize,
                DEFAULT_BREAK_ON_SENTENCE,
            ),
            last_input_end: None,
            frontier: None,
        }
    }
}

impl State {
    fn reset_all(&mut self, settings: &Settings) {
        *self = Self {
            window: Window::new(
                settings.columns.max(1) as usize,
                settings.lines.max(1) as usize,
                settings.break_on_sentence,
            ),
            ..Self::default()
        };
    }

    /// A new segment is a discontinuity. Carrying the old window across it
    /// would stamp stale text onto an unrelated timeline.
    fn reset_timeline(&mut self) {
        self.window.clear();
        self.last_input_end = None;
        self.frontier = None;
    }

    fn reset_display(&mut self) {
        self.window.clear();
        self.last_input_end = None;
    }

    fn apply_settings(&mut self, settings: &Settings) {
        self.window.set_columns(settings.columns.max(1) as usize);
        self.window.set_lines(settings.lines.max(1) as usize);
        self.window
            .set_break_on_sentence(settings.break_on_sentence);
    }

    fn clear_deadline(&self, settings: &Settings) -> Option<gst::ClockTime> {
        (settings.clear_after_ms > 0).then_some(())?;
        self.last_input_end
            .map(|end| end + gst::ClockTime::from_mseconds(settings.clear_after_ms as u64))
    }
}

/// Work to push after releasing the state lock. A downstream queue may block;
/// it must not prevent an application thread from changing element settings.
#[derive(Debug)]
enum Out {
    Gap(gst::ClockTime, Option<gst::ClockTime>),
    Cue(gst::ClockTime, gst::ClockTime, String),
}

pub struct TextRollup {
    srcpad: gst::Pad,
    sinkpad: gst::Pad,
    settings: Mutex<Settings>,
    state: Mutex<State>,
}

impl TextRollup {
    fn flush(&self, out: Vec<Out>) -> Result<gst::FlowSuccess, gst::FlowError> {
        for item in out {
            match item {
                Out::Gap(start, duration) => {
                    gst::trace!(CAT, imp = self, "gap at {start} for {duration:?}");
                    let mut builder = gst::event::Gap::builder(start);
                    if let Some(duration) = duration {
                        builder = builder.duration(duration);
                    }
                    self.srcpad.push_event(builder.build());
                }
                Out::Cue(pts, duration, text) => {
                    gst::log!(CAT, imp = self, "state at {pts} for {duration} {text:?}");
                    let mut buffer = gst::Buffer::from_mut_slice(text.into_bytes());
                    {
                        let buffer = buffer.get_mut().unwrap();
                        buffer.set_pts(pts);
                        buffer.set_duration(duration);
                    }
                    self.srcpad.push(buffer)?;
                }
            }
        }
        Ok(gst::FlowSuccess::Ok)
    }

    /// Emit the authoritative blank state and forget the old roll-up window.
    fn queue_clear(state: &mut State, out: &mut Vec<Out>, at: gst::ClockTime) {
        gst::debug!(CAT, "clearing the display at {at}");
        out.push(Out::Cue(at, gst::ClockTime::ZERO, String::new()));
        state.reset_display();
    }

    /// If media time has crossed the display deadline, publish the missed
    /// clear before accepting another state. This covers sources that omit GAP
    /// events without making the result depend on how quickly buffers arrive.
    fn clear_before_word(
        state: &mut State,
        settings: &Settings,
        out: &mut Vec<Out>,
        pts: gst::ClockTime,
    ) {
        if let Some(deadline) = state.clear_deadline(settings) {
            if pts >= deadline {
                Self::queue_clear(state, out, deadline);
            }
        }
    }

    fn sink_chain(
        &self,
        _pad: &gst::Pad,
        buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let pts = buffer.pts().ok_or_else(|| {
            gst::error!(CAT, imp = self, "need timestamped buffers");
            gst::FlowError::Error
        })?;
        let map = buffer.map_readable().map_err(|_| {
            gst::error!(CAT, imp = self, "can't map buffer readable");
            gst::FlowError::Error
        })?;
        let data = std::str::from_utf8(map.as_slice()).map_err(|err| {
            gst::error!(CAT, imp = self, "invalid utf8: {err}");
            gst::FlowError::Error
        })?;

        let settings = self.settings.lock().unwrap().clone();
        let mut out = Vec::new();
        {
            let mut state = self.state.lock().unwrap();
            if data.trim().is_empty() {
                // Empty UTF-8 is an explicit display transition, not silence.
                Self::queue_clear(&mut state, &mut out, pts);
            } else {
                let duration = buffer.duration().ok_or_else(|| {
                    gst::error!(CAT, imp = self, "non-empty caption buffers need duration");
                    gst::FlowError::Error
                })?;
                if duration.is_zero() {
                    gst::error!(CAT, imp = self, "caption duration must be non-zero");
                    return Err(gst::FlowError::Error);
                }
                Self::clear_before_word(&mut state, &settings, &mut out, pts);

                let mut changed = false;
                for word in data.split_whitespace() {
                    changed |= matches!(state.window.push_word(word), Push::Redraw(_));
                }

                if changed {
                    // A multi-word input is one committed update: downstream
                    // sees only the final complete window at the input timing.
                    out.push(Out::Cue(pts, duration, state.window.render()));
                    state.last_input_end = Some(pts + duration);
                }
            }
        }

        self.flush(out)
    }

    fn handle_gap(
        &self,
        state: &mut State,
        settings: &Settings,
        out: &mut Vec<Out>,
        start: gst::ClockTime,
        duration: Option<gst::ClockTime>,
    ) {
        let end = start + duration.unwrap_or(gst::ClockTime::ZERO);
        if state.frontier.is_some_and(|frontier| end < frontier) {
            gst::warning!(CAT, imp = self, "ignoring regressing GAP ending at {end}");
            return;
        }
        state.frontier = Some(state.frontier.map_or(end, |frontier| frontier.max(end)));

        let Some(deadline) = state.clear_deadline(settings) else {
            out.push(Out::Gap(start, duration));
            return;
        };
        if end < deadline {
            out.push(Out::Gap(start, duration));
            return;
        }

        // Preserve full GAP coverage while inserting the clear at its exact
        // media timestamp. When the supplied GAP starts after the deadline,
        // emit the missed clear first and then forward the GAP unchanged.
        if start < deadline {
            out.push(Out::Gap(start, Some(deadline - start)));
        }
        Self::queue_clear(state, out, deadline);
        if end > deadline {
            let tail_start = start.max(deadline);
            out.push(Out::Gap(tail_start, Some(end - tail_start)));
        } else if duration.is_none() && start >= deadline {
            out.push(Out::Gap(start, None));
        }
    }

    fn sink_event(&self, pad: &gst::Pad, event: gst::Event) -> bool {
        use gst::EventView;

        match event.view() {
            EventView::StreamStart(_) => {
                let settings = self.settings.lock().unwrap().clone();
                self.state.lock().unwrap().reset_all(&settings);
                gst::Pad::event_default(pad, Some(&*self.obj()), event)
            }
            EventView::Segment(_) => {
                self.state.lock().unwrap().reset_timeline();
                gst::Pad::event_default(pad, Some(&*self.obj()), event)
            }
            EventView::FlushStop(_) => {
                let settings = self.settings.lock().unwrap().clone();
                self.state.lock().unwrap().reset_all(&settings);
                gst::Pad::event_default(pad, Some(&*self.obj()), event)
            }
            EventView::Gap(gap) => {
                let (pts, duration) = gap.get();
                let settings = self.settings.lock().unwrap().clone();
                let mut out = Vec::new();
                {
                    let mut state = self.state.lock().unwrap();
                    self.handle_gap(&mut state, &settings, &mut out, pts, duration);
                }
                if let Err(err) = self.flush(out) {
                    gst::debug!(CAT, imp = self, "downstream returned {err:?} on GAP");
                }
                true
            }
            EventView::CustomDownstream(custom)
                if custom
                    .structure()
                    .is_some_and(|s| s.name() == "rstranscribe/final-transcript") =>
            {
                self.srcpad.push_event(event)
            }
            _ => gst::Pad::event_default(pad, Some(&*self.obj()), event),
        }
    }

    fn src_query(&self, pad: &gst::Pad, query: &mut gst::QueryRef) -> bool {
        use gst::QueryViewMut;

        match query.view_mut() {
            QueryViewMut::Latency(q) => {
                let mut upstream = gst::query::Latency::new();
                if !self.sinkpad.peer_query(&mut upstream) {
                    return false;
                }
                let (live, min, max) = upstream.result();
                q.set(live, min, max);
                true
            }
            _ => gst::Pad::query_default(pad, Some(&*self.obj()), query),
        }
    }
}

#[glib::object_subclass]
impl ObjectSubclass for TextRollup {
    const NAME: &'static str = "GstTextRollup";
    type Type = super::TextRollup;
    type ParentType = gst::Element;

    fn with_class(klass: &Self::Class) -> Self {
        let sinkpad = gst::Pad::builder_from_template(&klass.pad_template("sink").unwrap())
            .chain_function(|pad, parent, buffer| {
                TextRollup::catch_panic_pad_function(
                    parent,
                    || Err(gst::FlowError::Error),
                    |imp| imp.sink_chain(pad, buffer),
                )
            })
            .event_function(|pad, parent, event| {
                TextRollup::catch_panic_pad_function(
                    parent,
                    || false,
                    |imp| imp.sink_event(pad, event),
                )
            })
            .flags(gst::PadFlags::PROXY_CAPS | gst::PadFlags::FIXED_CAPS)
            .build();

        let srcpad = gst::Pad::builder_from_template(&klass.pad_template("src").unwrap())
            .query_function(|pad, parent, query| {
                TextRollup::catch_panic_pad_function(
                    parent,
                    || false,
                    |imp| imp.src_query(pad, query),
                )
            })
            .flags(gst::PadFlags::PROXY_CAPS | gst::PadFlags::FIXED_CAPS)
            .build();

        Self {
            srcpad,
            sinkpad,
            settings: Mutex::new(Settings::default()),
            state: Mutex::new(State::default()),
        }
    }
}

impl ObjectImpl for TextRollup {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
                glib::ParamSpecUInt::builder("columns")
                    .nick("Columns")
                    .blurb("Maximum display width per line")
                    .minimum(1)
                    .default_value(DEFAULT_COLUMNS)
                    .mutable_playing()
                    .build(),
                glib::ParamSpecUInt::builder("lines")
                    .nick("Lines")
                    .blurb("Number of lines in the roll-up window")
                    .minimum(1)
                    .default_value(DEFAULT_LINES)
                    .mutable_playing()
                    .build(),
                glib::ParamSpecUInt::builder("clear-after")
                    .nick("Clear After")
                    .blurb(
                        "Media-time silence after the previous input end before emitting an explicit clear, in milliseconds; 0 disables",
                    )
                    .default_value(DEFAULT_CLEAR_AFTER_MS)
                    .mutable_playing()
                    .build(),
                glib::ParamSpecBoolean::builder("break-on-sentence")
                    .nick("Break On Sentence")
                    .blurb(
                        "Finish the current line at sentence-final punctuation even if it is not full",
                    )
                    .default_value(DEFAULT_BREAK_ON_SENTENCE)
                    .mutable_playing()
                    .build(),
            ]
        });
        PROPERTIES.as_ref()
    }

    fn constructed(&self) {
        self.parent_constructed();
        let obj = self.obj();
        obj.add_pad(&self.sinkpad).unwrap();
        obj.add_pad(&self.srcpad).unwrap();
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        let mut settings = self.settings.lock().unwrap();
        let mut geometry_changed = false;
        match pspec.name() {
            "columns" => {
                settings.columns = value.get().unwrap();
                geometry_changed = true;
            }
            "lines" => {
                settings.lines = value.get().unwrap();
                geometry_changed = true;
            }
            "clear-after" => settings.clear_after_ms = value.get().unwrap(),
            "break-on-sentence" => {
                settings.break_on_sentence = value.get().unwrap();
                geometry_changed = true;
            }
            _ => unimplemented!(),
        }
        let snapshot = settings.clone();
        drop(settings);
        if geometry_changed {
            self.state.lock().unwrap().apply_settings(&snapshot);
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        let settings = self.settings.lock().unwrap();
        match pspec.name() {
            "columns" => settings.columns.to_value(),
            "lines" => settings.lines.to_value(),
            "clear-after" => settings.clear_after_ms.to_value(),
            "break-on-sentence" => settings.break_on_sentence.to_value(),
            _ => unimplemented!(),
        }
    }
}

impl GstObjectImpl for TextRollup {}

impl ElementImpl for TextRollup {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "Text Rollup",
                "Text/Filter",
                "Turn word-level text into a fixed roll-up caption window",
                "Elliott Darfink <elliott.darfink@gmail.com>",
            )
        });
        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let caps = gst::Caps::builder("text/x-raw")
                .field("format", "utf8")
                .build();
            let sink = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &caps,
            )
            .unwrap();
            let src = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &caps,
            )
            .unwrap();
            vec![src, sink]
        });
        PAD_TEMPLATES.as_ref()
    }

    fn change_state(
        &self,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        if transition == gst::StateChange::PausedToReady {
            let settings = self.settings.lock().unwrap().clone();
            self.state.lock().unwrap().reset_all(&settings);
        }
        self.parent_change_state(transition)
    }
}
