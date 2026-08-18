// SPDX-License-Identifier: MPL-2.0

/**
 * SECTION:element-transcribecpptranscriber
 *
 * Speech-to-text element using [transcribe.cpp], supporting every model family
 * that library ships with.
 *
 * The element runs in one of two modes, selected by the mode property:
 *
 * - `stream` uses the native streaming API. Every incoming buffer is fed to the
 *   model and the family's stable-prefix implementation decides when a piece of
 *   text stops changing. Only committed text is pushed on the src pad, so the
 *   output is append-only; the volatile hypothesis is available through the
 *   `partial-transcript` signal for UI use.
 * - `chunked` runs offline inference over a sliding window, for families that
 *   have no streaming path (whisper, canary, ...). It is the higher-latency
 *   mode and needs `chunk-duration` / `live-edge-offset` tuning.
 *
 * `auto` (the default) picks `stream` when the loaded model advertises
 * streaming support and `chunked` otherwise.
 *
 * The element requires mono F32 audio at the model's native sample rate — put
 * `audioconvert ! audioresample` in front of it. The rate is taken from the
 * model's own capabilities, so no property configures it.
 *
 * Streaming example, a Moonshine streaming model:
 *
 * ```shell
 * gst-launch-1.0 filesrc location=speech.wav ! wavparse ! audioconvert ! \
 *   audioresample ! clocksync ! \
 *   transcribecpptranscriber model-path=moonshine-tiny-streaming.gguf ! \
 *   fakesink dump=true
 * ```
 *
 * Chunked example, a whisper model:
 *
 * ```shell
 * gst-launch-1.0 filesrc location=speech.wav ! wavparse ! audioconvert ! \
 *   audioresample ! \
 *   transcribecpptranscriber model-path=whisper-large-v3.gguf mode=chunked \
 *     chunk-duration=4000 live-edge-offset=1000 ! \
 *   fakesink dump=true
 * ```
 *
 * [transcribe.cpp]: https://github.com/handy-computer/transcribe.cpp
 */
use std::sync::{LazyLock, Mutex, mpsc};

use byte_slice_cast::*;
use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use transcribe_cpp::{
    ModelOptions, MoonshineStreamingOptions, ParakeetBufferedStreamOptions, ParakeetStreamOptions,
    RunExtension, RunOptions, SessionOptions, StreamExtension, StreamOptions,
    VoxtralRealtimeStreamOptions, WhisperRunOptions,
};

use super::vad;
use super::worker::{self, Cmd, Ev, ModelInfo};
use super::{Backend, CommitPolicy, Itn, KvType, Mode, Overrun, Pnc, Task, Timestamps};

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "transcribecpptranscriber",
        gst::DebugColorFlags::empty(),
        Some("transcribe.cpp speech-to-text element"),
    )
});

const DEFAULT_LATENCY_MS: u32 = 1_000;
const DEFAULT_CHUNK_DURATION_MS: u32 = 4_000;
const DEFAULT_LIVE_EDGE_OFFSET_MS: u32 = 1_000;
const DEFAULT_QUEUE_SIZE: u32 = 32;
const DEFAULT_DISCONT_THRESHOLD_MS: u32 = 500;
/// How long the VAD gate may withhold audio before giving up and opening.
///
/// This is the pipeline's stall bound, so it is deliberately generous enough
/// to cover a speaker who joins late and short enough that a source which is
/// silent by nature still starts transcribing promptly.
const DEFAULT_VAD_MAX_WAIT_MS: u32 = 30_000;
/// Priming silence fed ahead of the first speech, per NVIDIA's guidance for
/// the Nemotron/Parakeet streaming checkpoints.
const DEFAULT_WARMUP_PAD_MS: u32 = 80;
/// How much audio to average over before judging real-time performance.
const REALTIME_WINDOW_MS: u64 = 5_000;
/// Shortest text buffer worth pushing. Roughly one frame at 24fps, and one
/// step of the 80ms grid the streaming families align to, so flooring to it
/// rarely collides with the next word.
const MIN_BUFFER_DURATION: gst::ClockTime = gst::ClockTime::from_mseconds(40);
/// Smallest advance worth a gap event. An RTP source delivers 20ms buffers, and
/// one event apiece would be pure noise downstream.
const GAP_GRANULARITY: gst::ClockTime = gst::ClockTime::from_mseconds(200);

#[derive(Debug, Clone)]
struct Settings {
    model_path: Option<String>,
    mode: Mode,
    latency_ms: u32,
    chunk_duration_ms: u32,
    live_edge_offset_ms: u32,
    queue_size: u32,
    overrun: Overrun,
    discont_threshold_ms: u32,
    /// Withhold audio until speech is detected, so a streaming model never
    /// opens its stream on silence. Zero disables the gate.
    vad_max_wait_ms: u32,
    /// Zero-filled priming chunk fed before the first speech. Zero disables it.
    warmup_pad_ms: u32,

    backend: Backend,
    gpu_device: i32,

    n_threads: i32,
    kv_type: KvType,
    n_ctx: i32,

    task: Task,
    language: Option<String>,
    target_language: Option<String>,
    timestamps: Timestamps,
    pnc: Pnc,
    itn: Itn,
    keep_special_tags: bool,
    spec_k_drafts: i32,

    commit_policy: CommitPolicy,
    stable_prefix_agreement_n: u32,

    family_options: Option<gst::Structure>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            model_path: None,
            mode: Mode::default(),
            latency_ms: DEFAULT_LATENCY_MS,
            chunk_duration_ms: DEFAULT_CHUNK_DURATION_MS,
            live_edge_offset_ms: DEFAULT_LIVE_EDGE_OFFSET_MS,
            queue_size: DEFAULT_QUEUE_SIZE,
            overrun: Overrun::default(),
            discont_threshold_ms: DEFAULT_DISCONT_THRESHOLD_MS,
            vad_max_wait_ms: DEFAULT_VAD_MAX_WAIT_MS,
            warmup_pad_ms: DEFAULT_WARMUP_PAD_MS,
            backend: Backend::default(),
            gpu_device: 0,
            n_threads: 0,
            kv_type: KvType::default(),
            n_ctx: 0,
            task: Task::default(),
            language: None,
            target_language: None,
            timestamps: Timestamps::default(),
            pnc: Pnc::default(),
            itn: Itn::default(),
            keep_special_tags: false,
            spec_k_drafts: -1,
            commit_policy: CommitPolicy::default(),
            stable_prefix_agreement_n: 0,
            family_options: None,
        }
    }
}

impl Settings {
    fn model_options(&self) -> ModelOptions {
        ModelOptions {
            backend: self.backend.into(),
            gpu_device: self.gpu_device,
        }
    }

    fn session_options(&self) -> SessionOptions {
        SessionOptions {
            n_threads: self.n_threads,
            kv_type: self.kv_type.into(),
            n_ctx: self.n_ctx,
        }
    }

    fn run_options(&self) -> Result<RunOptions, String> {
        Ok(RunOptions {
            task: self.task.into(),
            timestamps: self.timestamps.into(),
            pnc: self.pnc.into(),
            itn: self.itn.into(),
            language: self.language.clone(),
            target_language: self.target_language.clone(),
            keep_special_tags: self.keep_special_tags,
            spec_k_drafts: self.spec_k_drafts,
            family: self
                .family_options
                .as_ref()
                .map(run_extension)
                .transpose()?
                .flatten(),
        })
    }

    fn stream_options(&self) -> Result<StreamOptions, String> {
        Ok(StreamOptions {
            commit_policy: self.commit_policy.into(),
            stable_prefix_agreement_n: self.stable_prefix_agreement_n,
            family: self
                .family_options
                .as_ref()
                .map(stream_extension)
                .transpose()?
                .flatten(),
        })
    }
}

/// Read an optional typed field, ignoring a field of the wrong type rather
/// than failing the whole structure.
fn field<T: for<'a> glib::value::FromValue<'a> + 'static>(
    s: &gst::Structure,
    name: &str,
) -> Option<T> {
    s.get_optional::<T>(name).ok().flatten()
}

/// The run-slot extension named by `family-options`, if it names one.
///
/// The structure name selects the family, which is also how the two parakeet
/// streaming modes are told apart:
/// `family-options="parakeet-buffered,chunk-ms=640"`.
fn run_extension(s: &gst::Structure) -> Result<Option<RunExtension>, String> {
    match s.name().as_str() {
        "whisper" => Ok(Some(RunExtension::Whisper(WhisperRunOptions {
            initial_prompt: field(s, "initial-prompt"),
            condition_on_prev_tokens: field(s, "condition-on-prev-tokens"),
            temperature: field(s, "temperature"),
            temperature_inc: field(s, "temperature-inc"),
            compression_ratio_thold: field(s, "compression-ratio-thold"),
            logprob_thold: field(s, "logprob-thold"),
            no_speech_thold: field(s, "no-speech-thold"),
            max_prev_context_tokens: field(s, "max-prev-context-tokens"),
            seed: field(s, "seed"),
            max_initial_timestamp: field(s, "max-initial-timestamp"),
        }))),
        "parakeet-stream" | "parakeet-buffered" | "moonshine-streaming" | "voxtral-realtime" => {
            Ok(None)
        }
        other => Err(format!("unknown family-options family '{other}'")),
    }
}

/// The stream-slot extension named by `family-options`, if it names one.
fn stream_extension(s: &gst::Structure) -> Result<Option<StreamExtension>, String> {
    match s.name().as_str() {
        "parakeet-stream" => Ok(Some(StreamExtension::ParakeetStream(
            ParakeetStreamOptions {
                att_context_right: field(s, "att-context-right"),
            },
        ))),
        "parakeet-buffered" => Ok(Some(StreamExtension::ParakeetBuffered(
            ParakeetBufferedStreamOptions {
                left_ms: field(s, "left-ms"),
                chunk_ms: field(s, "chunk-ms"),
                right_ms: field(s, "right-ms"),
            },
        ))),
        "moonshine-streaming" => Ok(Some(StreamExtension::MoonshineStreaming(
            MoonshineStreamingOptions {
                min_decode_interval_ms: field(s, "min-decode-interval-ms"),
            },
        ))),
        "voxtral-realtime" => Ok(Some(StreamExtension::VoxtralRealtime(
            VoxtralRealtimeStreamOptions {
                num_delay_tokens: field(s, "num-delay-tokens"),
                min_decode_interval_ms: field(s, "min-decode-interval-ms"),
            },
        ))),
        "whisper" => Ok(None),
        other => Err(format!("unknown family-options family '{other}'")),
    }
}

#[derive(Default)]
struct State {
    info: Option<ModelInfo>,
    /// (live, min, max) as reported upstream.
    upstream_latency: Option<(bool, gst::ClockTime, Option<gst::ClockTime>)>,
    /// Running time of the first sample of the current stream. Word times are
    /// relative to it.
    base_pts: Option<gst::ClockTime>,
    /// End of the last buffer or gap pushed on the src pad.
    out_pts: Option<gst::ClockTime>,
    /// Stream-relative milliseconds the family has finalized, from
    /// [`Ev::Progress`]. Only ever moves forward within one stream.
    committed_ms: i64,
    /// Where the next input buffer is expected, for discontinuity detection.
    next_pts: Option<gst::ClockTime>,
    dropped: u64,
    /// Whether inference is currently slower than real time, and the running
    /// totals the verdict is drawn from.
    behind: bool,
    compute_ms: u64,
    audio_ms: u64,
    /// Withholds audio until speech starts, so the model never opens its
    /// stream on silence. Built once the native rate is known; `None` when the
    /// gate is disabled.
    gate: Option<vad::Gate>,
}

impl State {
    /// Reset everything tied to the current stream, keeping the model info.
    fn reset_timeline(&mut self) {
        self.base_pts = None;
        self.out_pts = None;
        self.next_pts = None;
        // The worker starts a fresh stream, which is cold in exactly the way
        // the gate exists to protect against, so re-arm it with the timeline.
        if let Some(gate) = self.gate.as_mut() {
            gate.reset();
        }
        // The worker restarts its stream too, so its committed milliseconds
        // begin again from zero. Keeping the old value would suppress every gap
        // until the new stream passed the old one's end.
        self.committed_ms = 0;
    }
}

pub struct Transcriber {
    srcpad: gst::Pad,
    sinkpad: gst::Pad,
    settings: Mutex<Settings>,
    state: Mutex<State>,
    /// Command side of the worker. Never held across a blocking operation.
    worker: Mutex<Option<worker::Handle>>,
    /// Event side. Only the streaming thread touches it, so it is fine to hold
    /// this lock across a blocking `recv`.
    events: Mutex<Option<mpsc::Receiver<Ev>>>,
}

impl Transcriber {
    fn upstream_latency(&self) -> Option<(bool, gst::ClockTime, Option<gst::ClockTime>)> {
        if let Some(latency) = self.state.lock().unwrap().upstream_latency {
            return Some(latency);
        }

        let mut peer_query = gst::query::Latency::new();
        if !self.sinkpad.peer_query(&mut peer_query) {
            gst::trace!(CAT, imp = self, "could not query upstream latency");
            return None;
        }

        let upstream_latency = peer_query.result();
        gst::info!(CAT, imp = self, "upstream latency: {upstream_latency:?}");
        self.state.lock().unwrap().upstream_latency = Some(upstream_latency);

        Some(upstream_latency)
    }

    fn is_live(&self) -> bool {
        self.upstream_latency()
            .map(|(live, _, _)| live)
            .unwrap_or(false)
    }

    /// Block until the worker reports the model is loaded.
    ///
    /// Called from the caps handler and from the first chain call, so model
    /// loading overlaps with pipeline setup instead of blocking the state
    /// change.
    fn ensure_ready(&self) -> Result<ModelInfo, gst::FlowError> {
        if let Some(info) = self.state.lock().unwrap().info.clone() {
            return Ok(info);
        }

        let events = self.events.lock().unwrap();
        let Some(events) = events.as_ref() else {
            return Err(gst::FlowError::Flushing);
        };

        loop {
            match events.recv() {
                Ok(Ev::Ready(info)) => {
                    let info = *info;
                    gst::info!(
                        CAT,
                        imp = self,
                        "loaded {} model (variant '{}') on {} backend, native rate {} Hz, \
                         alignment {}, mode {}",
                        info.arch,
                        info.variant,
                        info.backend,
                        info.native_sample_rate,
                        info.max_timestamp_kind,
                        if info.streaming { "stream" } else { "chunked" },
                    );
                    let was_streaming = {
                        let mut state = self.state.lock().unwrap();
                        let was = state.info.as_ref().map(|info| info.streaming);
                        // The gate needs the native rate, which only the
                        // loaded model can supply. Chunked inference re-reads
                        // its whole window every run and does not carry the
                        // cold-start damage, so it is left ungated.
                        let max_wait_ms = self.settings.lock().unwrap().vad_max_wait_ms;
                        let warmup_pad_ms = self.settings.lock().unwrap().warmup_pad_ms;
                        state.gate = (info.streaming && max_wait_ms > 0).then(|| {
                            let mut gate = vad::Gate::new(
                                info.native_sample_rate.max(1) as u64,
                                max_wait_ms as u64,
                            );
                            gate.set_warmup_pad_ms(warmup_pad_ms as u64);
                            gate
                        });
                        state.info = Some(info.clone());
                        was
                    };

                    // Until the model was loaded `our_latency` had to guess how
                    // `auto` would resolve, and it guesses streaming. A chunked
                    // model turns 1s into `latency + chunk-duration +
                    // live-edge-offset`, and nothing re-queries a latency that
                    // changed on its own - so say so.
                    if was_streaming != Some(info.streaming) && !info.streaming {
                        self.post_latency_changed();
                    }
                    return Ok(info);
                }
                Ok(Ev::Error(err)) => {
                    gst::element_imp_error!(self, gst::ResourceError::NotFound, ["{err}"]);
                    return Err(gst::FlowError::Error);
                }
                // Nothing else can precede Ready.
                Ok(_) => continue,
                Err(_) => return Err(gst::FlowError::Error),
            }
        }
    }

    /// Send a command, applying the overrun policy to audio only. Control
    /// commands are never dropped.
    fn send_cmd(&self, cmd: Cmd) -> Result<(), gst::FlowError> {
        let cmd_tx = {
            let worker = self.worker.lock().unwrap();
            let Some(handle) = worker.as_ref() else {
                return Err(gst::FlowError::Flushing);
            };
            handle.cmd_tx.clone()
        };

        let drop_on_overrun =
            matches!(cmd, Cmd::Feed(_)) && self.settings.lock().unwrap().overrun == Overrun::Drop;

        if drop_on_overrun {
            match cmd_tx.try_send(cmd) {
                Ok(()) => Ok(()),
                Err(mpsc::TrySendError::Full(_)) => {
                    let mut state = self.state.lock().unwrap();
                    state.dropped += 1;
                    let dropped = state.dropped;
                    drop(state);
                    gst::warning!(
                        CAT,
                        imp = self,
                        "inference is behind, dropped {dropped} buffer(s) so far"
                    );
                    Ok(())
                }
                Err(mpsc::TrySendError::Disconnected(_)) => Err(self.worker_died()),
            }
        } else {
            cmd_tx.send(cmd).map_err(|_| self.worker_died())
        }
    }

    /// The worker is gone. It sent its reason before exiting, so drain that out
    /// and report it — otherwise the pipeline dies with a bare flow error and
    /// no explanation anywhere.
    fn worker_died(&self) -> gst::FlowError {
        self.drain_events(None, true)
            .err()
            .unwrap_or(gst::FlowError::Error)
    }

    /// Push newly committed words as timestamped text buffers.
    ///
    /// Holes between words become gap events so downstream text consumers can
    /// advance their clock, and utterance boundaries become
    /// `rstranscribe/final-transcript` events — see [`Self::push_final_transcript`].
    fn push_words(
        &self,
        words: Vec<super::commit::TimedWord>,
        is_final: bool,
    ) -> Result<(), gst::FlowError> {
        if words.is_empty() {
            if is_final {
                self.push_final_transcript();
            }
            return Ok(());
        }

        let (base_pts, out_pts) = {
            let state = self.state.lock().unwrap();
            let Some(base_pts) = state.base_pts else {
                gst::debug!(
                    CAT,
                    imp = self,
                    "no base PTS yet, dropping {} word(s)",
                    words.len()
                );
                return Ok(());
            };
            (base_pts, state.out_pts.unwrap_or(base_pts))
        };

        for output in plan_timed_text(base_pts, out_pts, words) {
            match output {
                PlannedOutput::Gap { pts, duration } => {
                    let _ = self
                        .srcpad
                        .push_event(gst::event::Gap::builder(pts).duration(duration).build());
                }
                PlannedOutput::Word {
                    pts,
                    duration,
                    text,
                    ends_utterance,
                } => {
                    gst::log!(CAT, imp = self, "pushing {text:?} at {pts} ({duration})");
                    let mut buffer = gst::Buffer::from_slice(text);
                    {
                        let buffer = buffer.get_mut().unwrap();
                        buffer.set_pts(pts);
                        buffer.set_duration(duration);
                    }

                    let end = pts + duration;
                    self.state.lock().unwrap().out_pts = Some(end);
                    self.srcpad.push(buffer)?;

                    // The model punctuated the end of an utterance. Downstream
                    // formatters use the standard event to close their group.
                    if ends_utterance {
                        self.push_final_transcript();
                    }
                }
            }
        }

        if is_final {
            self.push_final_transcript();
        }

        Ok(())
    }

    /// Close the timeline up to what the family has finalized.
    ///
    /// Text is a sparse stream: a pause produces no buffers at all. A muxer
    /// aggregating this pad with audio and video has nothing to advance on and
    /// stalls waiting for a buffer that silence will never produce, so the
    /// contract for sparse streams is to say "nothing here" explicitly.
    ///
    /// The frontier comes from the worker and already excludes anything still
    /// pending, so a gap can never be published over a word that has yet to go
    /// out.
    fn advance_timeline(&self, committed_ms: i64) {
        let gap = {
            let mut state = self.state.lock().unwrap();

            // A family that aligns nothing gets one buffer per commit, each
            // running from the end of the last to the family's current edge.
            // Those already tile the timeline, so there is no silence to
            // declare, and a gap would only strand the span that follows it.
            if state
                .info
                .as_ref()
                .is_some_and(|info| info.max_timestamp_kind == "none")
            {
                return;
            }

            if committed_ms <= state.committed_ms {
                return;
            }
            state.committed_ms = committed_ms;

            // No audio has been timestamped yet, so there is no timeline to
            // close; the first buffer establishes it.
            let Some(base_pts) = state.base_pts else {
                return;
            };

            let frontier = base_pts + gst::ClockTime::from_mseconds(committed_ms.max(0) as u64);
            let out_pts = state.out_pts.unwrap_or(base_pts);

            // One event per incoming buffer would be noise; downstream only
            // needs to know the position moved.
            if frontier < out_pts + GAP_GRANULARITY {
                return;
            }

            state.out_pts = Some(frontier);
            (out_pts, frontier - out_pts)
        };

        gst::trace!(CAT, imp = self, "gap at {} for {}", gap.0, gap.1);
        let _ = self
            .srcpad
            .push_event(gst::event::Gap::builder(gap.0).duration(gap.1).build());
    }

    /// Publish a gap over audio the gate is withholding.
    ///
    /// The stream has not started as far as the model is concerned, so there
    /// is no `base_pts` yet and [`Self::advance_timeline`] has nothing to work
    /// from. Downstream does not care about that distinction: it needs to know
    /// the position moved, or a sparse-stream aggregator waits on this pad
    /// forever. The gate's own deadline bounds how long this can last, but
    /// that deadline is tens of seconds - far too long to leave a live mux
    /// with nothing at all.
    fn advance_gate_timeline(&self, start: gst::ClockTime, position: gst::ClockTime) {
        let gap = {
            let mut state = self.state.lock().unwrap();

            // Families that align nothing tile the timeline with their own
            // buffers, and a gap would only strand the span that follows.
            if state
                .info
                .as_ref()
                .is_some_and(|info| info.max_timestamp_kind == "none")
            {
                return;
            }

            // Nothing has been published yet, so the timeline starts at the
            // first audio the gate ever saw rather than at the model's
            // eventual zero.
            let from = *state.out_pts.get_or_insert(start);
            if position < from + GAP_GRANULARITY {
                return;
            }
            state.out_pts = Some(position);
            (from, position - from)
        };

        gst::trace!(CAT, imp = self, "gate gap at {} for {}", gap.0, gap.1);
        let _ = self
            .srcpad
            .push_event(gst::event::Gap::builder(gap.0).duration(gap.1).build());
    }

    /// Tell downstream that the transcript up to here will not change.
    ///
    /// This is the `rstranscribe/final-transcript` convention the gst-plugins-rs
    /// transcribers use, and what `textaccumulate` drains on — so grouping words
    /// into readable cues is a matter of adding `textaccumulate` to the
    /// pipeline rather than anything this element has to decide.
    fn push_final_transcript(&self) {
        gst::log!(CAT, imp = self, "marking transcript final");
        let _ = self.srcpad.push_event(
            gst::event::CustomDownstream::builder(
                gst::Structure::builder("rstranscribe/final-transcript").build(),
            )
            .build(),
        );
    }

    /// Consume worker events.
    ///
    /// With `until` set this blocks until that marker arrives; otherwise it
    /// takes whatever is already queued and returns. `discard` throws results
    /// away instead of pushing them, for the flush path where the src pad is
    /// not accepting data.
    fn drain_events(&self, until: Option<Marker>, discard: bool) -> Result<(), gst::FlowError> {
        let events = self.events.lock().unwrap();
        let Some(events) = events.as_ref() else {
            return Err(gst::FlowError::Flushing);
        };

        loop {
            let received = if until.is_some() {
                events.recv().map_err(|_| mpsc::TryRecvError::Disconnected)
            } else {
                events.try_recv()
            };

            let event = match received {
                Ok(event) => event,
                Err(mpsc::TryRecvError::Empty) => return Ok(()),
                Err(mpsc::TryRecvError::Disconnected) => {
                    // The worker exited without saying why, or said why and we
                    // already reported it. Either way the pipeline is done.
                    gst::element_imp_error!(
                        self,
                        gst::StreamError::Failed,
                        ["inference worker stopped unexpectedly"]
                    );
                    return Err(gst::FlowError::Error);
                }
            };

            match event {
                Ev::Words { words, is_final } => {
                    if !discard {
                        gst::debug!(
                            CAT,
                            imp = self,
                            "{} committed word(s){}",
                            words.len(),
                            if is_final { ", final" } else { "" }
                        );

                        self.push_words(words, is_final)?;
                    }
                }
                Ev::Partial(text) => {
                    if !discard && !text.is_empty() {
                        self.obj()
                            .emit_by_name::<()>("partial-transcript", &[&text]);
                    }
                }
                Ev::Progress { committed_ms } => {
                    if !discard {
                        self.advance_timeline(committed_ms);
                    }
                }
                Ev::Compute {
                    compute_ms,
                    audio_ms,
                } => self.report_compute(compute_ms, audio_ms),
                Ev::Drained if until == Some(Marker::Drained) => return Ok(()),
                Ev::Restarted if until == Some(Marker::Restarted) => return Ok(()),
                Ev::Drained | Ev::Restarted | Ev::Ready(_) => (),
                Ev::Error(err) => {
                    gst::element_imp_error!(self, gst::StreamError::Failed, ["{err}"]);
                    return Err(gst::FlowError::Error);
                }
            }
        }
    }

    /// Log how inference is tracking against real time.
    ///
    /// Spending more wall-clock time than the audio is long means a live
    /// pipeline falls behind without bound, which is worth a warning. It is
    /// only worth it in aggregate though: an RTP source delivers 20ms buffers,
    /// and per-call overhead makes any single one of those look catastrophic
    /// while the stream as a whole keeps up comfortably. So accumulate over a
    /// few seconds of audio, and only speak up when the verdict changes.
    fn report_compute(&self, compute_ms: u64, audio_ms: u64) {
        gst::log!(
            CAT,
            imp = self,
            "inference took {compute_ms}ms for {audio_ms}ms of audio"
        );

        if !self.is_live() {
            return;
        }

        let verdict = {
            let mut state = self.state.lock().unwrap();
            state.compute_ms += compute_ms;
            state.audio_ms += audio_ms;

            if state.audio_ms < REALTIME_WINDOW_MS {
                None
            } else {
                let (compute_ms, audio_ms) = (state.compute_ms, state.audio_ms);
                state.compute_ms = 0;
                state.audio_ms = 0;

                let behind = compute_ms > audio_ms;
                let was_behind = std::mem::replace(&mut state.behind, behind);
                (behind != was_behind).then_some((behind, compute_ms, audio_ms))
            }
        };

        match verdict {
            Some((true, compute_ms, audio_ms)) => gst::warning!(
                CAT,
                imp = self,
                "inference is slower than real time ({compute_ms}ms of compute for {audio_ms}ms \
                 of audio); the pipeline will fall behind"
            ),
            Some((false, compute_ms, audio_ms)) => gst::info!(
                CAT,
                imp = self,
                "inference is keeping up again ({compute_ms}ms of compute for {audio_ms}ms of audio)"
            ),
            None => (),
        }
    }

    /// Hand the gate's withheld audio to the model, if it is still waiting.
    ///
    /// Called wherever a stream ends. Without this a stream that is quiet
    /// throughout - a held meeting, a muted speaker - would be dropped
    /// entirely rather than transcribed as best the model can.
    fn release_gate(&self) -> Result<(), gst::FlowError> {
        let released = {
            let mut state = self.state.lock().unwrap();
            let Some(gate) = state.gate.as_mut() else {
                return Ok(());
            };
            if gate.is_open() {
                return Ok(());
            }
            let Some(vad::Decision::Open { audio, .. }) = gate.take_held() else {
                return Ok(());
            };

            // No speech was ever detected, so the timeline starts where the
            // withheld audio does.
            if state.base_pts.is_none() {
                let start = state.next_pts.unwrap_or(gst::ClockTime::ZERO);
                let rate = state
                    .info
                    .as_ref()
                    .map(|info| info.native_sample_rate.max(1) as u64)
                    .unwrap_or(1);
                let start = start
                    .checked_sub(samples_duration(audio.len() as u64, rate))
                    .unwrap_or(gst::ClockTime::ZERO);
                state.base_pts = Some(start);
                state.out_pts = Some(start);
            }
            audio
        };

        gst::debug!(
            CAT,
            imp = self,
            "stream ended without speech, feeding {} withheld sample(s)",
            released.len()
        );
        self.send_cmd(Cmd::Feed(released))
    }

    /// Flush the model's buffered audio and push whatever it produces.
    fn finalize(&self) -> Result<(), gst::FlowError> {
        // The gate may still be holding the whole stream, waiting for speech
        // that is now never going to arrive. Quiet audio is still audio the
        // user wants a transcript for, so release it instead of dropping it.
        self.release_gate()?;

        if self.state.lock().unwrap().base_pts.is_none() {
            return Ok(());
        }

        gst::debug!(CAT, imp = self, "finalizing stream");
        self.send_cmd(Cmd::Finalize)?;
        self.drain_events(Some(Marker::Drained), false)
    }

    /// Throw away model state and rebase the timeline. Used after a flush or a
    /// discontinuity, where the audio either side is unrelated.
    fn restart(&self, discard: bool) -> Result<(), gst::FlowError> {
        gst::debug!(CAT, imp = self, "restarting stream");
        self.send_cmd(Cmd::Restart)?;
        self.drain_events(Some(Marker::Restarted), discard)?;
        self.state.lock().unwrap().reset_timeline();
        Ok(())
    }

    fn sink_chain(
        &self,
        pad: &gst::Pad,
        buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        gst::log!(CAT, obj = pad, "handling {buffer:?}");

        let info = self.ensure_ready()?;
        let rate = info.native_sample_rate.max(1) as u64;

        let Some(pts) = buffer.pts() else {
            gst::element_imp_error!(self, gst::StreamError::Failed, ["need timestamped buffers"]);
            return Err(gst::FlowError::Error);
        };

        let Ok(data) = buffer.map_readable() else {
            gst::element_imp_error!(
                self,
                gst::StreamError::Failed,
                ["failed to map buffer readable"]
            );
            return Err(gst::FlowError::Error);
        };

        let Ok(samples) = data.as_slice_of::<f32>() else {
            gst::element_imp_error!(self, gst::StreamError::Failed, ["misaligned audio buffer"]);
            return Err(gst::FlowError::Error);
        };

        let discont_threshold = gst::ClockTime::from_mseconds(
            self.settings.lock().unwrap().discont_threshold_ms as u64,
        );

        // A jump in the input timeline means the audio on either side is
        // unrelated: flush what the model has, then rebase. Only meaningful
        // once a stream is running — the first buffer of every stream carries
        // the DISCONT flag, and there is nothing to flush at that point.
        let discont = {
            let state = self.state.lock().unwrap();
            match state.next_pts {
                Some(next_pts) => {
                    pts > next_pts + discont_threshold
                        || pts + discont_threshold < next_pts
                        || buffer.flags().contains(gst::BufferFlags::DISCONT)
                }
                None => false,
            }
        };

        if discont {
            gst::debug!(CAT, imp = self, "discontinuity at {pts}");
            self.finalize()?;
            self.restart(false)?;
        }

        let buffer_end = pts + samples_duration(samples.len() as u64, rate);

        // Feeding a streaming family silence as its very first audio can
        // poison the stream for far longer than the silence itself, so hold
        // the head of the stream back until someone speaks. Everything after
        // the gate opens flows straight through.
        let (warmup, pcm) = {
            let mut state = self.state.lock().unwrap();

            let decision = match state.gate.as_mut() {
                Some(gate) => gate.push(samples),
                None => vad::Decision::Passthrough,
            };

            // Input timing is tracked whatever the gate decides, so a
            // discontinuity is judged against the audio that actually arrived
            // rather than against what the model happened to be shown.
            state.next_pts = Some(buffer_end);

            let (armed_at, warmup, pcm) = match decision {
                vad::Decision::Withhold => {
                    gst::trace!(CAT, imp = self, "withholding {pts}, waiting for speech");
                    drop(state);
                    // Downstream still has to be able to advance. Text is a
                    // sparse stream, so a muxer aggregating this pad has
                    // nothing to wait on but our gap events - and holding
                    // audio back must not turn into holding the *timeline*
                    // back, or a live mux starves for as long as nobody
                    // speaks.
                    self.advance_gate_timeline(pts, buffer_end);
                    return Ok(gst::FlowSuccess::Ok);
                }
                vad::Decision::Passthrough => (pts, Vec::new(), samples.to_vec()),
                vad::Decision::Open {
                    skipped_samples,
                    audio,
                    warmup,
                } => {
                    // The gate can reach back into buffers it already
                    // withheld, so the audio it releases starts before this
                    // buffer does. Anchor to where the gate was armed and let
                    // the skip carry the rest.
                    let armed_at = buffer_end
                        .checked_sub(samples_duration(skipped_samples + audio.len() as u64, rate))
                        .unwrap_or(gst::ClockTime::ZERO);
                    (
                        armed_at + samples_duration(skipped_samples, rate),
                        warmup,
                        audio,
                    )
                }
            };

            // The timeline is anchored to the first sample the *model* sees,
            // not the first sample that arrived. Both zeros move by the same
            // amount, so word times still land on the audio they describe.
            if state.base_pts.is_none() {
                // The priming chunk is audio as far as the model is concerned,
                // so its stream clock starts there rather than at the first
                // real sample. Anchoring behind the pad by exactly its length
                // cancels that out; without this every word would be reported
                // one pad late.
                let base = armed_at
                    .checked_sub(samples_duration(warmup.len() as u64, rate))
                    .unwrap_or(gst::ClockTime::ZERO);
                gst::debug!(CAT, imp = self, "speech detected, feeding from {armed_at}");
                state.base_pts = Some(base);
                state.out_pts = Some(armed_at.max(state.out_pts.unwrap_or(armed_at)));
            }

            (warmup, pcm)
        };
        drop(data);

        if pcm.is_empty() {
            return Ok(gst::FlowSuccess::Ok);
        }

        // Priming silence goes in as its own chunk, ahead of the speech. The
        // model's stream-relative clock does advance by its length, but the
        // element's does not - and that is the point: `base_pts` describes the
        // first *real* sample, so the pad's timeline is unaffected.
        if !warmup.is_empty() {
            self.send_cmd(Cmd::Feed(warmup))?;
        }

        self.send_cmd(Cmd::Feed(pcm))?;
        self.drain_events(None, false)?;

        Ok(gst::FlowSuccess::Ok)
    }

    fn sink_event(&self, pad: &gst::Pad, event: gst::Event) -> bool {
        gst::log!(CAT, obj = pad, "handling event {event:?}");

        use gst::EventView::*;
        match event.view() {
            Caps(caps) => {
                let Ok(info) = self.ensure_ready() else {
                    return false;
                };

                let audio_info = match gst_audio::AudioInfo::from_caps(caps.caps()) {
                    Ok(audio_info) => audio_info,
                    Err(err) => {
                        gst::element_imp_error!(
                            self,
                            gst::CoreError::Negotiation,
                            ["invalid audio caps: {err}"]
                        );
                        return false;
                    }
                };

                if audio_info.rate() as i32 != info.native_sample_rate {
                    gst::element_imp_error!(
                        self,
                        gst::CoreError::Negotiation,
                        [
                            "model {} wants {} Hz but caps say {} Hz, insert audioresample",
                            info.arch,
                            info.native_sample_rate,
                            audio_info.rate()
                        ]
                    );
                    return false;
                }

                self.srcpad.push_event(
                    gst::event::Caps::builder(self.srcpad.pad_template().unwrap().caps())
                        .seqnum(event.seqnum())
                        .build(),
                )
            }
            FlushStart(_) => {
                // Abort any in-flight compute so the flush is not stuck behind
                // a full inference run.
                if let Some(handle) = self.worker.lock().unwrap().as_ref() {
                    handle.cancel.cancel();
                }
                gst::Pad::event_default(pad, Some(&*self.obj()), event)
            }
            FlushStop(_) => {
                // Make sure a concurrent push has returned before we touch the
                // timeline.
                let _ = self.sinkpad.stream_lock();

                if let Some(handle) = self.worker.lock().unwrap().as_ref() {
                    handle.cancel.reset();
                }

                {
                    let mut state = self.state.lock().unwrap();
                    state.reset_timeline();
                    state.dropped = 0;
                }

                // Results produced before the flush are stale; drop them.
                let _ = self.restart(true);

                gst::Pad::event_default(pad, Some(&*self.obj()), event)
            }
            Eos(_) | SegmentDone(_) => {
                let _ = self.finalize();
                gst::Pad::event_default(pad, Some(&*self.obj()), event)
            }
            Segment(_) => {
                let _ = self.finalize();
                let _ = self.restart(false);
                gst::Pad::event_default(pad, Some(&*self.obj()), event)
            }
            Gap(gap) => {
                let _ = self.finalize();
                let _ = self.restart(false);

                let (pts, duration) = gap.get();
                self.state.lock().unwrap().out_pts =
                    Some(pts + duration.unwrap_or(gst::ClockTime::ZERO));

                gst::Pad::event_default(pad, Some(&*self.obj()), event)
            }
            _ => gst::Pad::event_default(pad, Some(&*self.obj()), event),
        }
    }

    fn sink_query(&self, pad: &gst::Pad, query: &mut gst::QueryRef) -> bool {
        // Exactly one sample rate works, and only the model knows which, so
        // this is worth waiting for the load to finish: it is what lets
        // audioresample negotiate the right rate by itself instead of picking
        // one we then have to reject.
        if let gst::QueryViewMut::Caps(q) = query.view_mut() {
            let rate = self.ensure_ready().ok().map(|info| info.native_sample_rate);

            if let Some(rate) = rate {
                let caps = gst_audio::AudioCapsBuilder::new()
                    .format(AUDIO_FORMAT)
                    .rate(rate)
                    .channels(1)
                    .layout(gst_audio::AudioLayout::Interleaved)
                    .build();

                let result = match q.filter() {
                    Some(filter) => {
                        filter.intersect_with_mode(&caps, gst::CapsIntersectMode::First)
                    }
                    None => caps,
                };
                q.set_result(&result);
                return true;
            }
        }

        gst::Pad::query_default(pad, Some(&*self.obj()), query)
    }

    fn src_query(&self, pad: &gst::Pad, query: &mut gst::QueryRef) -> bool {
        gst::log!(CAT, obj = pad, "handling query {query:?}");

        match query.view_mut() {
            gst::QueryViewMut::Latency(q) => {
                // A latency query is exactly when upstream's answer may have
                // changed - an adapting rtpjitterbuffer recalculates the
                // pipeline for precisely this reason - so never serve the
                // cached figure here. Answering with a stale upstream minimum
                // under-reports this branch, and the muxer then advances past
                // buffers that were budgeted for.
                self.state.lock().unwrap().upstream_latency = None;

                let Some((live, min, max)) = self.upstream_latency() else {
                    return false;
                };

                if !live {
                    q.set(false, gst::ClockTime::ZERO, gst::ClockTime::NONE);
                    return true;
                }

                let our_latency = self.our_latency();
                gst::debug!(
                    CAT,
                    imp = self,
                    "reporting latency {our_latency} on top of upstream {min}"
                );
                q.set(live, our_latency + min, max.opt_add(our_latency));
                true
            }
            _ => gst::Pad::query_default(pad, Some(&*self.obj()), query),
        }
    }

    /// The latency this element adds.
    ///
    /// In chunked mode the window itself dominates. In streaming mode the
    /// commit lag is data-dependent — the family decides when a prefix is
    /// stable — so the latency property is the user's declared budget and the
    /// element warns when reality exceeds it.
    fn our_latency(&self) -> gst::ClockTime {
        let resolved_mode = self
            .state
            .lock()
            .unwrap()
            .info
            .as_ref()
            .map(|info| info.streaming);

        let settings = self.settings.lock().unwrap();
        // Before the model is loaded we do not know what auto will resolve to;
        // assume streaming, and re-report latency once we do know.
        let streaming = resolved_mode.unwrap_or(settings.mode != Mode::Chunked);

        let ms = if streaming {
            settings.latency_ms
        } else {
            settings.latency_ms + settings.chunk_duration_ms + settings.live_edge_offset_ms
        };

        gst::ClockTime::from_mseconds(ms as u64)
    }

    fn prepare(&self) -> Result<(), gst::ErrorMessage> {
        let settings = self.settings.lock().unwrap().clone();

        let Some(model_path) = settings.model_path.clone() else {
            return Err(gst::error_msg!(
                gst::CoreError::Failed,
                ["model-path property must be set"]
            ));
        };

        if settings.live_edge_offset_ms >= settings.chunk_duration_ms {
            return Err(gst::error_msg!(
                gst::CoreError::Failed,
                ["chunk-duration must be greater than live-edge-offset"]
            ));
        }

        let run_options = settings
            .run_options()
            .map_err(|err| gst::error_msg!(gst::CoreError::Failed, ["{err}"]))?;
        let stream_options = settings
            .stream_options()
            .map_err(|err| gst::error_msg!(gst::CoreError::Failed, ["{err}"]))?;

        let config = worker::Config {
            model_path,
            model_options: settings.model_options(),
            session_options: settings.session_options(),
            run_options,
            stream_options,
            mode: settings.mode,
            chunk_ms: settings.chunk_duration_ms as i64,
            live_edge_offset_ms: settings.live_edge_offset_ms as i64,
        };

        gst::debug!(CAT, imp = self, "preparing with {config:?}");
        self.post_start("loading", "loading model");

        let (handle, events) = worker::spawn(
            config,
            settings.queue_size.max(1) as usize,
            self.obj().upcast_ref::<gst::Element>().downgrade(),
        );
        *self.worker.lock().unwrap() = Some(handle);
        *self.events.lock().unwrap() = Some(events);

        Ok(())
    }

    fn unprepare(&self) {
        // Take the handle out before joining: shutdown blocks, and the worker
        // lock must stay available to whoever else wants to cancel.
        let handle = self.worker.lock().unwrap().take();
        if let Some(handle) = handle {
            handle.shutdown();
        }
        let _ = self.events.lock().unwrap().take();
        *self.state.lock().unwrap() = State::default();
    }

    /// Ask the pipeline to recalculate latency, because ours just changed.
    ///
    /// A latency query is only ever answered, never volunteered: nothing
    /// re-asks an element whose own figure moved. Without this the pipeline
    /// keeps whatever it collected at startup, and every buffer beyond that
    /// budget arrives after downstream has moved on.
    fn post_latency_changed(&self) {
        let obj = self.obj();
        gst::info!(
            CAT,
            imp = self,
            "latency changed, asking for a recalculation"
        );
        let _ = obj.post_message(gst::message::Latency::builder().src(&*obj).build());
    }

    fn post_start(&self, code: &str, text: &str) {
        let obj = self.obj();
        let _ = obj.post_message(
            gst::message::Progress::builder(gst::ProgressType::Start, code, text)
                .src(&*obj)
                .build(),
        );
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Marker {
    Drained,
    Restarted,
}

/// Running time spanned by `samples` at `rate`.
fn samples_duration(samples: u64, rate: u64) -> gst::ClockTime {
    gst::ClockTime::SECOND
        .mul_div_floor(samples, rate)
        .unwrap_or(gst::ClockTime::ZERO)
}

/// Whether this word closes a sentence.
///
/// Trailing quotes and brackets are skipped so `he said "no."` still counts.
/// The model's own punctuation is the only signal available — an abbreviation
/// is indistinguishable from a sentence end, which is why this only marks a
/// boundary rather than deciding anything.
fn ends_sentence(text: &str) -> bool {
    text.trim_end()
        .trim_end_matches(['"', '\'', ')', ']', '}', '”', '’', '»'])
        .ends_with(['.', '!', '?', '…'])
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PlannedOutput {
    Gap {
        pts: gst::ClockTime,
        duration: gst::ClockTime,
    },
    Word {
        pts: gst::ClockTime,
        duration: gst::ClockTime,
        text: String,
        ends_utterance: bool,
    },
}

/// Convert committed model rows into the standard sparse timed-text contract.
/// Keeping this calculation independent of streaming threads makes its
/// monotonicity and GAP behavior directly testable without loading a model.
fn plan_timed_text(
    base_pts: gst::ClockTime,
    mut out_pts: gst::ClockTime,
    words: Vec<super::commit::TimedWord>,
) -> Vec<PlannedOutput> {
    let next_starts: Vec<Option<i64>> = (0..words.len())
        .map(|index| words.get(index + 1).map(|next| next.t0_ms))
        .collect();
    let mut output = Vec::with_capacity(words.len() * 2);

    for (word, next_start) in words.into_iter().zip(next_starts) {
        let relative_start = gst::ClockTime::from_mseconds(word.t0_ms.max(0) as u64);
        let relative_end = gst::ClockTime::from_mseconds(word.t1_ms.max(0) as u64);
        let pts = (base_pts + relative_start).max(out_pts);
        let end = floor_end(
            pts,
            (base_pts + relative_end).max(pts),
            next_start.map(|next_start| {
                base_pts + gst::ClockTime::from_mseconds(next_start.max(0) as u64)
            }),
        );

        if pts > out_pts {
            output.push(PlannedOutput::Gap {
                pts: out_pts,
                duration: pts - out_pts,
            });
        }
        output.push(PlannedOutput::Word {
            pts,
            duration: end - pts,
            ends_utterance: ends_sentence(&word.text),
            text: word.text,
        });
        out_pts = end;
    }

    output
}

/// Where a word's buffer should end.
///
/// Models do hand back rows with `t0 == t1`, and a zero-length text buffer
/// means nothing to a renderer — textoverlay and subtitle muxers both need a
/// duration to decide how long to show it. So floor the span, preferring not to
/// run into the next word, but overlapping it rather than emitting nothing:
/// `out_pts` then nudges that next word later by at most the floor, which beats
/// pushing a buffer that displays for no time.
fn floor_end(
    pts: gst::ClockTime,
    end: gst::ClockTime,
    next_start: Option<gst::ClockTime>,
) -> gst::ClockTime {
    let floored = pts + MIN_BUFFER_DURATION;
    if end >= floored {
        return end;
    }

    match next_start {
        Some(next_start) if next_start > pts => floored.min(next_start),
        _ => floored,
    }
}

#[cfg(target_endian = "little")]
const AUDIO_FORMAT: gst_audio::AudioFormat = gst_audio::AudioFormat::F32le;
#[cfg(target_endian = "big")]
const AUDIO_FORMAT: gst_audio::AudioFormat = gst_audio::AudioFormat::F32be;

#[glib::object_subclass]
impl ObjectSubclass for Transcriber {
    const NAME: &'static str = "GstTranscribeCppTranscriber";
    type Type = super::Transcriber;
    type ParentType = gst::Element;

    fn with_class(klass: &Self::Class) -> Self {
        let templ = klass.pad_template("sink").unwrap();
        let sinkpad = gst::Pad::builder_from_template(&templ)
            .chain_function(|pad, parent, buffer| {
                Transcriber::catch_panic_pad_function(
                    parent,
                    || Err(gst::FlowError::Error),
                    |imp| imp.sink_chain(pad, buffer),
                )
            })
            .event_function(|pad, parent, event| {
                Transcriber::catch_panic_pad_function(
                    parent,
                    || false,
                    |imp| imp.sink_event(pad, event),
                )
            })
            .query_function(|pad, parent, query| {
                Transcriber::catch_panic_pad_function(
                    parent,
                    || false,
                    |imp| imp.sink_query(pad, query),
                )
            })
            .build();

        let templ = klass.pad_template("src").unwrap();
        let srcpad = gst::Pad::builder_from_template(&templ)
            .query_function(|pad, parent, query| {
                Transcriber::catch_panic_pad_function(
                    parent,
                    || false,
                    |imp| imp.src_query(pad, query),
                )
            })
            .flags(gst::PadFlags::FIXED_CAPS)
            .build();

        Self {
            srcpad,
            sinkpad,
            settings: Default::default(),
            state: Default::default(),
            worker: Default::default(),
            events: Default::default(),
        }
    }
}

impl ObjectImpl for Transcriber {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
                glib::ParamSpecString::builder("model-path")
                    .nick("Model Path")
                    .blurb("Path to a GGUF model understood by transcribe.cpp")
                    .build(),
                glib::ParamSpecEnum::builder_with_default("mode", Mode::default())
                    .nick("Mode")
                    .blurb(
                        "Whether to use the native streaming API or sliding-window \
                         offline inference",
                    )
                    .build(),
                glib::ParamSpecUInt::builder("latency")
                    .nick("Latency")
                    .blurb(
                        "The expected processing latency, in milliseconds. Counts towards \
                         the latency reported downstream. The element warns on a live \
                         pipeline when inference turns out to be slower than real time",
                    )
                    .default_value(DEFAULT_LATENCY_MS)
                    .build(),
                glib::ParamSpecUInt::builder("chunk-duration")
                    .nick("Chunk Duration")
                    .blurb(
                        "mode=chunked: new audio accumulated before each inference run, \
                         in milliseconds. Counts towards the reported latency",
                    )
                    .default_value(DEFAULT_CHUNK_DURATION_MS)
                    .build(),
                glib::ParamSpecUInt::builder("live-edge-offset")
                    .nick("Live Edge Offset")
                    .blurb(
                        "mode=chunked: trailing audio whose words are withheld as unstable, \
                         in milliseconds. Counts towards the reported latency",
                    )
                    .default_value(DEFAULT_LIVE_EDGE_OFFSET_MS)
                    .build(),
                glib::ParamSpecUInt::builder("queue-size")
                    .nick("Queue Size")
                    .blurb("How many audio buffers may be queued for inference")
                    .minimum(1)
                    .default_value(DEFAULT_QUEUE_SIZE)
                    .build(),
                glib::ParamSpecEnum::builder_with_default("overrun", Overrun::default())
                    .nick("Overrun")
                    .blurb("What to do when inference cannot keep up with a live source")
                    .build(),
                glib::ParamSpecUInt::builder("discont-threshold")
                    .nick("Discontinuity Threshold")
                    .blurb(
                        "A timeline jump larger than this (milliseconds) finalizes the \
                         current stream and starts a new one",
                    )
                    .default_value(DEFAULT_DISCONT_THRESHOLD_MS)
                    .build(),
                glib::ParamSpecUInt::builder("vad-max-wait")
                    .nick("VAD Max Wait")
                    .blurb(
                        "mode=stream: withhold audio until speech is detected, so the \
                         model never opens its stream on silence, giving up after this \
                         many milliseconds. 0 disables the gate",
                    )
                    .default_value(DEFAULT_VAD_MAX_WAIT_MS)
                    .build(),
                glib::ParamSpecUInt::builder("warmup-pad")
                    .nick("Warmup Pad")
                    .blurb(
                        "mode=stream: milliseconds of digital silence fed as a priming \
                         chunk before the first speech, which the streaming families \
                         need to avoid missing their opening words. 0 disables it",
                    )
                    .default_value(DEFAULT_WARMUP_PAD_MS)
                    .build(),
                glib::ParamSpecEnum::builder_with_default("backend", Backend::default())
                    .nick("Backend")
                    .blurb("Which compute backend to request")
                    .build(),
                glib::ParamSpecInt::builder("gpu-device")
                    .nick("GPU Device")
                    .blurb("GPU device registry index; 0 auto-selects, preferring discrete GPUs")
                    .minimum(0)
                    .default_value(0)
                    .build(),
                glib::ParamSpecInt::builder("n-threads")
                    .nick("Number of Threads")
                    .blurb("CPU threads for ops that run on CPU; 0 uses the library default")
                    .minimum(0)
                    .default_value(0)
                    .build(),
                glib::ParamSpecEnum::builder_with_default("kv-type", KvType::default())
                    .nick("K/V Type")
                    .blurb("K/V activation precision")
                    .build(),
                glib::ParamSpecInt::builder("n-ctx")
                    .nick("Context Size")
                    .blurb("Decoder context cap in tokens; 0 uses the model maximum")
                    .minimum(0)
                    .default_value(0)
                    .build(),
                glib::ParamSpecEnum::builder_with_default("task", Task::default())
                    .nick("Task")
                    .blurb("Whether to transcribe or translate")
                    .build(),
                glib::ParamSpecString::builder("language")
                    .nick("Language")
                    .blurb("Source language hint (ISO code); unset auto-detects")
                    .build(),
                glib::ParamSpecString::builder("target-language")
                    .nick("Target Language")
                    .blurb("Target language (ISO code) when task=translate")
                    .build(),
                glib::ParamSpecEnum::builder_with_default("timestamps", Timestamps::default())
                    .nick("Timestamps")
                    .blurb(
                        "Timestamp granularity to request. Word timestamps or finer give \
                         per-word output buffers; none falls back to one buffer per commit",
                    )
                    .build(),
                glib::ParamSpecEnum::builder_with_default("pnc", Pnc::default())
                    .nick("PNC")
                    .blurb("Punctuation and capitalization toggle, on supporting families")
                    .build(),
                glib::ParamSpecEnum::builder_with_default("itn", Itn::default())
                    .nick("ITN")
                    .blurb("Inverse text normalization toggle, on supporting families")
                    .build(),
                glib::ParamSpecBoolean::builder("keep-special-tags")
                    .nick("Keep Special Tags")
                    .blurb("Keep special vocabulary tags in the returned text")
                    .default_value(false)
                    .build(),
                glib::ParamSpecInt::builder("spec-k-drafts")
                    .nick("Speculative Drafts")
                    .blurb("Speculative-decode draft length; -1 family default, 0 disabled")
                    .minimum(-1)
                    .default_value(-1)
                    .build(),
                glib::ParamSpecEnum::builder_with_default("commit-policy", CommitPolicy::default())
                    .nick("Commit Policy")
                    .blurb("mode=stream: when committed text is allowed to grow")
                    .build(),
                glib::ParamSpecUInt::builder("stable-prefix-agreement-n")
                    .nick("Stable Prefix Agreement")
                    .blurb(
                        "mode=stream: consecutive agreeing hypotheses before a prefix \
                         commits; 0 uses the library default",
                    )
                    .default_value(0)
                    .build(),
                glib::ParamSpecBoxed::builder::<gst::Structure>("family-options")
                    .nick("Family Options")
                    .blurb(
                        "Family-specific knobs, as a structure named after the family: \
                         whisper, parakeet-stream, parakeet-buffered, moonshine-streaming \
                         or voxtral-realtime. Example: \
                         family-options=\"parakeet-buffered,chunk-ms=640\"",
                    )
                    .build(),
                glib::ParamSpecBoxed::builder::<gst::Structure>("model-info")
                    .nick("Model Info")
                    .blurb("What the loaded model reported about itself; unset until loaded")
                    .read_only()
                    .build(),
            ]
        });

        PROPERTIES.as_ref()
    }

    fn signals() -> &'static [glib::subclass::Signal] {
        static SIGNALS: LazyLock<Vec<glib::subclass::Signal>> = LazyLock::new(|| {
            vec![
                /**
                 * GstTranscribeCppTranscriber::partial-transcript:
                 * @text: the volatile hypothesis after the committed prefix
                 *
                 * Emitted when the model's tentative text changes. This text is
                 * rewritten freely and is never pushed on the src pad; it is
                 * meant for live UI. Emitted on the streaming thread, so
                 * handlers must not block.
                 */
                glib::subclass::Signal::builder("partial-transcript")
                    .param_types([String::static_type()])
                    .build(),
            ]
        });

        SIGNALS.as_ref()
    }

    fn constructed(&self) {
        self.parent_constructed();

        let obj = self.obj();
        obj.add_pad(&self.sinkpad).unwrap();
        obj.add_pad(&self.srcpad).unwrap();
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        let mut settings = self.settings.lock().unwrap();

        match pspec.name() {
            "model-path" => settings.model_path = value.get().unwrap(),
            "mode" => settings.mode = value.get().unwrap(),
            "latency" => settings.latency_ms = value.get().unwrap(),
            "chunk-duration" => settings.chunk_duration_ms = value.get().unwrap(),
            "live-edge-offset" => settings.live_edge_offset_ms = value.get().unwrap(),
            "queue-size" => settings.queue_size = value.get().unwrap(),
            "overrun" => settings.overrun = value.get().unwrap(),
            "discont-threshold" => settings.discont_threshold_ms = value.get().unwrap(),
            "vad-max-wait" => settings.vad_max_wait_ms = value.get().unwrap(),
            "warmup-pad" => settings.warmup_pad_ms = value.get().unwrap(),
            "backend" => settings.backend = value.get().unwrap(),
            "gpu-device" => settings.gpu_device = value.get().unwrap(),
            "n-threads" => settings.n_threads = value.get().unwrap(),
            "kv-type" => settings.kv_type = value.get().unwrap(),
            "n-ctx" => settings.n_ctx = value.get().unwrap(),
            "task" => settings.task = value.get().unwrap(),
            "language" => settings.language = value.get().unwrap(),
            "target-language" => settings.target_language = value.get().unwrap(),
            "timestamps" => settings.timestamps = value.get().unwrap(),
            "pnc" => settings.pnc = value.get().unwrap(),
            "itn" => settings.itn = value.get().unwrap(),
            "keep-special-tags" => settings.keep_special_tags = value.get().unwrap(),
            "spec-k-drafts" => settings.spec_k_drafts = value.get().unwrap(),
            "commit-policy" => settings.commit_policy = value.get().unwrap(),
            "stable-prefix-agreement-n" => {
                settings.stable_prefix_agreement_n = value.get().unwrap()
            }
            "family-options" => settings.family_options = value.get().unwrap(),
            _ => unimplemented!(),
        }

        // Dropped before posting: a bus handler is free to call back into this
        // element, and it would deadlock on a lock still held here.
        drop(settings);

        if matches!(
            pspec.name(),
            "latency" | "chunk-duration" | "live-edge-offset"
        ) {
            self.post_latency_changed();
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        if pspec.name() == "model-info" {
            let state = self.state.lock().unwrap();
            return state
                .info
                .as_ref()
                .map(|info| {
                    gst::Structure::builder("transcribe-cpp/model-info")
                        .field("arch", info.arch.as_str())
                        .field("variant", info.variant.as_str())
                        .field("backend", info.backend.as_str())
                        .field("native-sample-rate", info.native_sample_rate)
                        .field("max-audio-ms", info.max_audio_ms)
                        .field("max-timestamp-kind", info.max_timestamp_kind)
                        .field("supports-streaming", info.supports_streaming)
                        .field("supports-translate", info.supports_translate)
                        .field("streaming", info.streaming)
                        .field("languages", info.languages.join(",").as_str())
                        .build()
                })
                .to_value();
        }

        let settings = self.settings.lock().unwrap();

        match pspec.name() {
            "model-path" => settings.model_path.to_value(),
            "mode" => settings.mode.to_value(),
            "latency" => settings.latency_ms.to_value(),
            "chunk-duration" => settings.chunk_duration_ms.to_value(),
            "live-edge-offset" => settings.live_edge_offset_ms.to_value(),
            "queue-size" => settings.queue_size.to_value(),
            "overrun" => settings.overrun.to_value(),
            "discont-threshold" => settings.discont_threshold_ms.to_value(),
            "vad-max-wait" => settings.vad_max_wait_ms.to_value(),
            "warmup-pad" => settings.warmup_pad_ms.to_value(),
            "backend" => settings.backend.to_value(),
            "gpu-device" => settings.gpu_device.to_value(),
            "n-threads" => settings.n_threads.to_value(),
            "kv-type" => settings.kv_type.to_value(),
            "n-ctx" => settings.n_ctx.to_value(),
            "task" => settings.task.to_value(),
            "language" => settings.language.to_value(),
            "target-language" => settings.target_language.to_value(),
            "timestamps" => settings.timestamps.to_value(),
            "pnc" => settings.pnc.to_value(),
            "itn" => settings.itn.to_value(),
            "keep-special-tags" => settings.keep_special_tags.to_value(),
            "spec-k-drafts" => settings.spec_k_drafts.to_value(),
            "commit-policy" => settings.commit_policy.to_value(),
            "stable-prefix-agreement-n" => settings.stable_prefix_agreement_n.to_value(),
            "family-options" => settings.family_options.to_value(),
            _ => unimplemented!(),
        }
    }
}

impl GstObjectImpl for Transcriber {}

impl ElementImpl for Transcriber {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "Transcriber",
                "Text/Audio/Filter",
                "Speech to text filter, using transcribe.cpp",
                "Elliott Darfink <elliott.darfink@gmail.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            // The rate is whatever the loaded model wants; the sink caps query
            // narrows this to the exact value once we know it.
            let sink_caps = gst_audio::AudioCapsBuilder::new()
                .format(AUDIO_FORMAT)
                .rate_range(8_000..=192_000)
                .channels(1)
                .layout(gst_audio::AudioLayout::Interleaved)
                .build();
            let sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &sink_caps,
            )
            .unwrap();

            let src_caps = gst::Caps::builder("text/x-raw")
                .field("format", "utf8")
                .build();
            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &src_caps,
            )
            .unwrap();

            vec![src_pad_template, sink_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }

    fn change_state(
        &self,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        gst::info!(CAT, imp = self, "changing state {transition:?}");

        match transition {
            gst::StateChange::NullToReady => {
                self.prepare().map_err(|err| {
                    self.post_error_message(err);
                    gst::StateChangeError
                })?;
            }
            gst::StateChange::PausedToReady => {
                // Do not let a long inference run hold up the state change.
                if let Some(handle) = self.worker.lock().unwrap().as_ref() {
                    handle.cancel.cancel();
                }
            }
            _ => (),
        }

        let ret = self.parent_change_state(transition)?;

        if transition == gst::StateChange::ReadyToNull {
            self.unprepare();
        }

        Ok(ret)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(value: u64) -> gst::ClockTime {
        gst::ClockTime::from_mseconds(value)
    }

    fn word(text: &str, start_ms: i64, end_ms: i64) -> super::super::commit::TimedWord {
        super::super::commit::TimedWord {
            text: text.to_owned(),
            t0_ms: start_ms,
            t1_ms: end_ms,
        }
    }

    #[test]
    fn sentence_ends_are_recognised() {
        assert!(ends_sentence("done."));
        assert!(ends_sentence("really?"));
        assert!(ends_sentence("wow!"));
        assert!(ends_sentence("\"no.\""));
        assert!(ends_sentence("trailing. "));
        assert!(!ends_sentence("poets,"));
        assert!(!ends_sentence("mid"));
        // An abbreviation is indistinguishable from a sentence end here; the
        // model's punctuation is all we have to go on.
        assert!(ends_sentence("Dr."));
    }

    #[test]
    fn a_long_enough_span_is_left_alone() {
        assert_eq!(floor_end(ms(1_000), ms(1_240), Some(ms(1_240))), ms(1_240));
    }

    #[test]
    fn a_zero_length_span_gets_the_floor() {
        assert_eq!(floor_end(ms(1_000), ms(1_000), None), ms(1_040));
        assert_eq!(floor_end(ms(1_000), ms(1_000), Some(ms(1_500))), ms(1_040));
    }

    #[test]
    fn the_floor_yields_to_a_closer_next_word() {
        // Flooring to 40ms would run past a word starting 20ms later, which
        // would shove that word forward. Take the 20ms instead.
        assert_eq!(floor_end(ms(1_000), ms(1_000), Some(ms(1_020))), ms(1_020));
    }

    #[test]
    fn two_rows_on_the_same_instant_overlap_rather_than_vanish() {
        assert_eq!(floor_end(ms(1_000), ms(1_000), Some(ms(1_000))), ms(1_040));
    }

    #[test]
    fn timed_text_plan_is_utf8_monotonic_non_overlapping_and_nonzero() {
        let output = plan_timed_text(
            ms(10_000),
            ms(10_000),
            vec![
                word("Héj", 100, 180),
                word("世界", 180, 180),
                word("again.", 220, 300),
            ],
        );

        let words: Vec<_> = output
            .iter()
            .filter_map(|item| match item {
                PlannedOutput::Word {
                    pts,
                    duration,
                    text,
                    ends_utterance,
                } => Some((*pts, *duration, text.as_str(), *ends_utterance)),
                PlannedOutput::Gap { .. } => None,
            })
            .collect();
        assert_eq!(words.len(), 3);
        assert!(words.iter().all(|(_, duration, text, _)| {
            !duration.is_zero() && std::str::from_utf8(text.as_bytes()).is_ok()
        }));
        for pair in words.windows(2) {
            assert!(pair[0].0 + pair[0].1 <= pair[1].0);
        }
        assert!(!words[0].3);
        assert!(words[2].3, "final-transcript follows sentence punctuation");
    }

    #[test]
    fn initial_and_internal_holes_are_explicit_gaps() {
        let output = plan_timed_text(
            ms(1_000),
            ms(1_000),
            vec![word("one", 200, 300), word("two", 600, 700)],
        );
        assert_eq!(
            output,
            vec![
                PlannedOutput::Gap {
                    pts: ms(1_000),
                    duration: ms(200),
                },
                PlannedOutput::Word {
                    pts: ms(1_200),
                    duration: ms(100),
                    text: "one".into(),
                    ends_utterance: false,
                },
                PlannedOutput::Gap {
                    pts: ms(1_300),
                    duration: ms(300),
                },
                PlannedOutput::Word {
                    pts: ms(1_600),
                    duration: ms(100),
                    text: "two".into(),
                    ends_utterance: false,
                },
            ]
        );
    }

    #[test]
    fn multiword_commit_preserves_every_row_in_order() {
        let output = plan_timed_text(
            gst::ClockTime::ZERO,
            gst::ClockTime::ZERO,
            vec![
                word("one", 0, 50),
                word("two", 50, 100),
                word("three", 100, 150),
            ],
        );
        let text: Vec<_> = output
            .iter()
            .filter_map(|item| match item {
                PlannedOutput::Word { text, .. } => Some(text.as_str()),
                PlannedOutput::Gap { .. } => None,
            })
            .collect();
        assert_eq!(text, ["one", "two", "three"]);
    }

    #[test]
    fn source_pad_advertises_official_timed_text_caps() {
        gst::init().unwrap();
        let template = Transcriber::pad_templates()
            .iter()
            .find(|template| template.name_template() == "src")
            .unwrap();
        assert!(
            template.caps().can_intersect(
                &gst::Caps::builder("text/x-raw")
                    .field("format", "utf8")
                    .build()
            )
        );
    }
}
