// SPDX-License-Identifier: MPL-2.0

//! Pad-observed keepalive and stall watchdog for sparse streams.
//!
//! A muxer aggregating this pad cannot close a cluster while the pad is
//! empty, and a non-live aggregator has no timeout — silence on one pad
//! stalls the whole publication. This element watches its sink pad and
//! guarantees it is never empty for longer than `keepalive-ms`: it emits a
//! GAP event at the observed frontier on a timer, and — when upstream has
//! been silent for `stall-ms` — it advances the frontier at the tick cadence
//! so a dead upstream cannot pin the mux forever.
//!
//! It does not interpret the stream: buffers pass through untouched, and
//! upstream GAP events are consumed (this element owns gap emission
//! downstream) after their spans are folded into the frontier.

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "gapkeeper",
        gst::DebugColorFlags::empty(),
        Some("Sparse-pad keepalive and stall watchdog"),
    )
});

const DEFAULT_KEEPALIVE_MS: u32 = 0;
const DEFAULT_STALL_MS: u32 = 0;

#[derive(Debug, Clone)]
struct Settings {
    keepalive_ms: u32,
    stall_ms: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            keepalive_ms: DEFAULT_KEEPALIVE_MS,
            stall_ms: DEFAULT_STALL_MS,
        }
    }
}

#[derive(Default)]
struct State {
    /// End of the last buffer or gap observed on the sink pad (media time).
    frontier: Option<gst::ClockTime>,
    /// Start of the current segment, for anchoring before the first event.
    segment_start: Option<gst::ClockTime>,
    /// Wall clock of the last upstream word or gap, for the stall detector.
    last_upstream_at: Option<Instant>,
}

impl State {
    fn reset(&mut self) {
        *self = State::default();
    }
}

pub struct GapKeeper {
    srcpad: gst::Pad,
    sinkpad: gst::Pad,
    settings: Mutex<Settings>,
    state: Mutex<State>,
    worker: Mutex<Option<KeepaliveWorker>>,
}

struct KeepaliveWorker {
    stop: Arc<AtomicBool>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl GapKeeper {
    fn observe(&self, pts: gst::ClockTime, duration: gst::ClockTime) {
        let mut state = self.state.lock().unwrap();
        state.last_upstream_at = Some(Instant::now());
        let end = pts.opt_add(duration);
        let candidate = end.unwrap_or(pts);
        if state.frontier.is_none() || candidate > state.frontier.unwrap() {
            state.frontier = Some(candidate);
        }
    }

    fn keepalive_tick(&self) {
        let settings = self.settings.lock().unwrap();
        let interval_ms = settings.keepalive_ms;
        if interval_ms == 0 {
            return;
        }
        let stall_ms = settings.stall_ms;
        drop(settings);
        if self.obj().current_state() != gst::State::Playing {
            return;
        }
        let mut state = self.state.lock().unwrap();
        let Some(mut anchor) = state.frontier.or(state.segment_start) else {
            return;
        };
        let stalled = stall_ms > 0
            && state
                .last_upstream_at
                .is_some_and(|last| last.elapsed() >= Duration::from_millis(stall_ms as u64));
        if stalled {
            anchor += gst::ClockTime::from_mseconds(interval_ms as u64);
            state.frontier = Some(anchor);
            gst::warning!(
                CAT,
                imp = self,
                "no upstream word or gap for {stall_ms}ms; force-advancing frontier to {anchor}"
            );
        }
        // Deliberately no duration: a wall-clock tick has no evidence for how
        // long the silence will last.
        let event = gst::event::Gap::builder(anchor).build();
        gst::trace!(CAT, imp = self, "keepalive gap at {anchor}");
        let _ = self.srcpad.push_event(event);
    }

    fn arm_keepalive(&self) {
        self.disarm_keepalive();
        let interval_ms = self.settings.lock().unwrap().keepalive_ms;
        if interval_ms == 0 {
            return;
        }
        let weak = self.downgrade();
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let join = std::thread::Builder::new()
            .name("gapkeeper-keepalive".into())
            .spawn(move || {
                while !worker_stop.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(interval_ms as u64));
                    if worker_stop.load(Ordering::Relaxed) {
                        break;
                    }
                    let Some(imp) = weak.upgrade() else {
                        break;
                    };
                    imp.keepalive_tick();
                }
            })
            .expect("spawn keepalive worker");
        *self.worker.lock().unwrap() = Some(KeepaliveWorker {
            stop,
            join: Some(join),
        });
    }

    fn disarm_keepalive(&self) {
        if let Some(worker) = self.worker.lock().unwrap().take() {
            worker.stop.store(true, Ordering::Relaxed);
            if let Some(join) = worker.join {
                let _ = join.join();
            }
        }
    }

    fn sink_chain(
        &self,
        _pad: &gst::Pad,
        buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let (pts, duration) = (buffer.pts(), buffer.duration());
        if let Some(pts) = pts {
            self.observe(pts, duration.unwrap_or(gst::ClockTime::ZERO));
        }
        self.srcpad.push(buffer)
    }

    fn sink_event(&self, pad: &gst::Pad, event: gst::Event) -> bool {
        use gst::EventView;

        match event.view() {
            EventView::StreamStart(_) => {
                self.state.lock().unwrap().reset();
                gst::Pad::event_default(pad, Some(&*self.obj()), event)
            }
            EventView::Segment(segment) => {
                let mut state = self.state.lock().unwrap();
                state.reset();
                state.segment_start = match segment.segment().start() {
                    gst::GenericFormattedValue::Time(start) => start,
                    _ => None,
                };
                drop(state);
                gst::Pad::event_default(pad, Some(&*self.obj()), event)
            }
            EventView::FlushStop(_) => {
                self.state.lock().unwrap().reset();
                gst::Pad::event_default(pad, Some(&*self.obj()), event)
            }
            EventView::Gap(gap) => {
                let (pts, duration) = gap.get();
                self.observe(pts, duration.unwrap_or(gst::ClockTime::ZERO));
                // Consumed: we own gap emission downstream.
                true
            }
            EventView::CustomDownstream(custom) => {
                if custom
                    .structure()
                    .is_some_and(|s| s.name() == "rstranscribe/final-transcript")
                {
                    return self.srcpad.push_event(event);
                }
                gst::Pad::event_default(pad, Some(&*self.obj()), event)
            }
            _ => gst::Pad::event_default(pad, Some(&*self.obj()), event),
        }
    }
}

#[glib::object_subclass]
impl ObjectSubclass for GapKeeper {
    const NAME: &'static str = "GstGapKeeper";
    type Type = super::GapKeeper;
    type ParentType = gst::Element;

    fn with_class(klass: &Self::Class) -> Self {
        let templ = klass.pad_template("sink").unwrap();
        let sinkpad = gst::Pad::builder_from_template(&templ)
            .chain_function(|pad, parent, buffer| {
                GapKeeper::catch_panic_pad_function(
                    parent,
                    || Err(gst::FlowError::Error),
                    |imp| imp.sink_chain(pad, buffer),
                )
            })
            .event_function(|pad, parent, event| {
                GapKeeper::catch_panic_pad_function(
                    parent,
                    || false,
                    |imp| imp.sink_event(pad, event),
                )
            })
            .flags(gst::PadFlags::PROXY_CAPS | gst::PadFlags::FIXED_CAPS)
            .build();

        let templ = klass.pad_template("src").unwrap();
        let srcpad = gst::Pad::builder_from_template(&templ)
            .flags(gst::PadFlags::PROXY_CAPS | gst::PadFlags::FIXED_CAPS)
            .build();

        Self {
            srcpad,
            sinkpad,
            settings: Mutex::new(Settings::default()),
            state: Mutex::new(State::default()),
            worker: Mutex::new(None),
        }
    }
}

impl ObjectImpl for GapKeeper {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
                glib::ParamSpecUInt::builder("keepalive-ms")
                    .nick("Keepalive Interval")
                    .blurb(
                        "While playing, emit a GAP event at the observed frontier every this \
                         many milliseconds so downstream muxers never wait on this sparse \
                         pad. 0 = off",
                    )
                    .default_value(DEFAULT_KEEPALIVE_MS)
                    .mutable_playing()
                    .build(),
                glib::ParamSpecUInt::builder("stall-ms")
                    .nick("Stall Advance")
                    .blurb(
                        "When no upstream word or gap has arrived for this many milliseconds, \
                         advance the frontier at the keepalive cadence so a dead upstream \
                         cannot pin a downstream mux. 0 = off",
                    )
                    .default_value(DEFAULT_STALL_MS)
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

    fn dispose(&self) {
        self.disarm_keepalive();
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        let mut settings = self.settings.lock().unwrap();
        match pspec.name() {
            "keepalive-ms" => {
                settings.keepalive_ms = value.get().unwrap();
            }
            "stall-ms" => {
                settings.stall_ms = value.get().unwrap();
            }
            _ => unimplemented!(),
        }
        let keepalive_ms = settings.keepalive_ms;
        drop(settings);
        // Re-arm so a property change takes effect while playing.
        if keepalive_ms > 0 {
            self.arm_keepalive();
        } else {
            self.disarm_keepalive();
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        let settings = self.settings.lock().unwrap();
        match pspec.name() {
            "keepalive-ms" => settings.keepalive_ms.to_value(),
            "stall-ms" => settings.stall_ms.to_value(),
            _ => unimplemented!(),
        }
    }
}

impl GstObjectImpl for GapKeeper {}

impl ElementImpl for GapKeeper {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "Gap Keeper",
                "Text/Filter",
                "Keep a sparse pad covered with GAP events so downstream aggregators never wait",
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
        if transition == gst::StateChange::ReadyToPaused {
            self.arm_keepalive();
        }
        if transition == gst::StateChange::PausedToReady {
            self.disarm_keepalive();
            self.state.lock().unwrap().reset();
        }

        self.parent_change_state(transition)
    }
}
