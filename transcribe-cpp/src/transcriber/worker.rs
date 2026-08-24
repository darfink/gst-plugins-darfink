// SPDX-License-Identifier: MPL-2.0

//! The inference thread.
//!
//! [`Session`] is `Send` but not `Sync`, and [`Stream`] mutably borrows the
//! session it came from. That pair cannot be stored in the element's state
//! struct — it is self-referential. Giving the session to a dedicated thread
//! and keeping the stream as a loop local sidesteps the problem entirely: the
//! borrow never has to outlive a stack frame.
//!
//! The element talks to this thread over a bounded command channel (the
//! backpressure knob) and reads results back over an unbounded event channel,
//! so every pad interaction still happens on the streaming thread.
//!
//! [`Stream`]: transcribe_cpp::Stream

use std::sync::mpsc;

use gst::glib;
use gst::prelude::*;
use transcribe_cpp::{
    CancelToken, Error, Model, ModelOptions, RunOptions, Session, SessionOptions, StreamOptions,
    Transcript,
};

use super::Mode;
use super::commit::{self, CommitTracker, TimedWord};

/// Window ceiling for a family that reports no maximum audio length.
const DEFAULT_MAX_WINDOW_MS: i64 = 30_000;

/// Everything the worker needs, captured when the element goes to READY.
#[derive(Debug, Clone)]
pub struct Config {
    pub model_path: String,
    pub model_options: ModelOptions,
    pub session_options: SessionOptions,
    pub run_options: RunOptions,
    pub stream_options: StreamOptions,
    pub mode: Mode,
    /// Chunked mode: new audio accumulated before each inference run.
    pub chunk_ms: i64,
    /// Chunked mode: trailing audio whose words are withheld as unstable.
    pub live_edge_offset_ms: i64,
}

/// What the element learned about the model once it was loaded.
#[derive(Debug, Clone)]
pub struct ModelInfo {
    pub native_sample_rate: i32,
    pub arch: String,
    pub variant: String,
    pub backend: String,
    pub languages: Vec<String>,
    pub max_audio_ms: i64,
    /// The finest alignment this family will ever emit. `none` means the
    /// element has to synthesize spans from the family's audio progress.
    pub max_timestamp_kind: &'static str,
    pub supports_streaming: bool,
    pub supports_translate: bool,
    /// Whether the element ended up streaming, after resolving [`Mode::Auto`].
    pub streaming: bool,
}

pub enum Cmd {
    /// 16 kHz-ish mono f32 samples, at the model's native rate.
    Feed(Vec<f32>),
    /// Flush buffered audio, emit the final text, then start a fresh stream.
    Finalize,
    /// Throw away the stream and start over (flush, seek).
    Restart,
    Stop,
}

pub enum Ev {
    Ready(Box<ModelInfo>),
    /// Newly committed words, in stream-relative milliseconds.
    Words {
        words: Vec<TimedWord>,
        is_final: bool,
    },
    /// The volatile suffix, for UI. Never pushed on the src pad.
    Partial(String),
    /// How far into the stream the family has finalized its decision, in
    /// stream-relative milliseconds. Monotonic.
    ///
    /// Text is sparse — silence produces no words at all — so without this the
    /// src pad would go quiet for as long as nobody speaks, and a downstream
    /// aggregator would have nothing to advance on. This is what lets the
    /// element emit gap events instead of nothing.
    Progress {
        committed_ms: i64,
    },
    /// Wall-clock cost of one native call against the audio it consumed. The
    /// element uses the ratio to tell whether it is keeping up with a live
    /// source.
    Compute {
        compute_ms: u64,
        audio_ms: u64,
    },
    /// Ack for [`Cmd::Restart`]: everything before it is stale.
    Restarted,
    /// Ack for [`Cmd::Finalize`]: the stream is fully drained.
    Drained,
    Error(String),
}

pub struct Handle {
    pub cmd_tx: mpsc::SyncSender<Cmd>,
    pub cancel: CancelToken,
    join: Option<std::thread::JoinHandle<()>>,
}

/// Publish a drained stream in timeline order.
///
/// Final words have to precede the final frontier: once downstream receives
/// the progress event it is entitled to treat every earlier instant as
/// decided silence. Conversely, omitting that frontier strands the model's
/// commit holdback whenever a discontinuity restarts the stream before EOS.
fn send_final_output(
    ev_tx: &mpsc::Sender<Ev>,
    words: Vec<TimedWord>,
    drained_ms: i64,
) -> Result<(), ()> {
    ev_tx
        .send(Ev::Words {
            words,
            is_final: true,
        })
        .map_err(|_| ())?;
    ev_tx
        .send(Ev::Progress {
            committed_ms: drained_ms,
        })
        .map_err(|_| ())
}

impl Handle {
    /// Stop the worker and wait for it. Cancels any in-flight compute first,
    /// so this does not block for the length of an inference run.
    pub fn shutdown(mut self) {
        self.cancel.cancel();
        let _ = self.cmd_tx.send(Cmd::Stop);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// Spawn the inference thread. Model loading happens on that thread, so a
/// multi-gigabyte model does not block the state change; the element waits for
/// [`Ev::Ready`] the first time it actually needs the model.
///
/// The event channel is unbounded so the worker can never block emitting a
/// result; backpressure lives entirely in the bounded command channel.
pub fn spawn(
    config: Config,
    queue_len: usize,
    element: glib::WeakRef<gst::Element>,
) -> (Handle, mpsc::Receiver<Ev>) {
    let (cmd_tx, cmd_rx) = mpsc::sync_channel(queue_len);
    let (ev_tx, ev_rx) = mpsc::channel();
    let cancel = CancelToken::new();

    let thread_cancel = cancel.clone();
    let join = std::thread::Builder::new()
        .name("transcribecpp-worker".into())
        .spawn(move || {
            if let Err(err) = run(config, thread_cancel, &element, &cmd_rx, &ev_tx) {
                post_progress(
                    &element,
                    gst::ProgressType::Canceled,
                    "failed to load model",
                );
                let _ = ev_tx.send(Ev::Error(err));
            }
        })
        .expect("failed to spawn worker thread");

    (
        Handle {
            cmd_tx,
            cancel,
            join: Some(join),
        },
        ev_rx,
    )
}

/// Post a "loading" progress message on the element's bus.
///
/// This has to come from the worker rather than from the streaming thread: a
/// live pipeline stays in PAUSED until every started progress finishes, and no
/// data flows until it reaches PLAYING. Completing the progress from a
/// dataflow-driven path would deadlock exactly that pipeline.
fn post_progress(element: &glib::WeakRef<gst::Element>, type_: gst::ProgressType, text: &str) {
    let Some(element) = element.upgrade() else {
        return;
    };
    let _ = element.post_message(
        gst::message::Progress::builder(type_, "loading", text)
            .src(&element)
            .build(),
    );
}

fn run(
    config: Config,
    cancel: CancelToken,
    element: &glib::WeakRef<gst::Element>,
    cmd_rx: &mpsc::Receiver<Cmd>,
    ev_tx: &mpsc::Sender<Ev>,
) -> Result<(), String> {
    let model = Model::load_with(&config.model_path, &config.model_options)
        .map_err(|err| format!("failed to load model {}: {err}", config.model_path))?;

    let caps = model.capabilities();
    let streaming = match config.mode {
        Mode::Auto => caps.supports_streaming,
        Mode::Stream => true,
        Mode::Chunked => false,
    };

    if streaming && !caps.supports_streaming {
        return Err(format!(
            "mode=stream requested but model {} ({}) does not support streaming",
            config.model_path,
            model.arch()
        ));
    }

    let info = ModelInfo {
        native_sample_rate: caps.native_sample_rate,
        arch: model.arch(),
        variant: model.variant(),
        backend: model.backend(),
        languages: caps.languages.clone(),
        max_audio_ms: caps.max_audio_ms,
        max_timestamp_kind: match caps.max_timestamp_kind {
            transcribe_cpp::TimestampKind::None => "none",
            transcribe_cpp::TimestampKind::Auto => "auto",
            transcribe_cpp::TimestampKind::Segment => "segment",
            transcribe_cpp::TimestampKind::Word => "word",
            transcribe_cpp::TimestampKind::Token => "token",
        },
        supports_streaming: caps.supports_streaming,
        supports_translate: caps.supports_translate,
        streaming,
    };
    let rate = info.native_sample_rate.max(1) as i64;
    // Whisper and friends cap a single run (30s for whisper); 0 means the
    // family has no practical limit, but an unbounded window would still be a
    // memory leak on silence, so keep a ceiling either way.
    let max_window_ms = if caps.max_audio_ms > 0 {
        caps.max_audio_ms
    } else {
        DEFAULT_MAX_WINDOW_MS
    };

    post_progress(element, gst::ProgressType::Complete, "loaded model");

    if ev_tx.send(Ev::Ready(Box::new(info))).is_err() {
        return Ok(());
    }

    let mut session = model
        .session_with(&config.session_options)
        .map_err(|err| format!("failed to open session: {err}"))?;
    session.set_cancel_token(&cancel);

    if streaming {
        run_streaming(&mut session, &config, rate, cmd_rx, ev_tx)
    } else {
        run_chunked(&mut session, &config, rate, max_window_ms, cmd_rx, ev_tx)
    }
}

/// Report what one native call cost against the audio it consumed.
fn report_compute(
    ev_tx: &mpsc::Sender<Ev>,
    started: std::time::Instant,
    audio_ms: u64,
) -> Result<(), ()> {
    ev_tx
        .send(Ev::Compute {
            compute_ms: started.elapsed().as_millis() as u64,
            audio_ms,
        })
        .map_err(|_| ())
}

/// Native streaming: one `feed` per incoming buffer, the family decides when a
/// prefix is stable.
fn run_streaming(
    session: &mut Session,
    config: &Config,
    rate: i64,
    cmd_rx: &mpsc::Receiver<Cmd>,
    ev_tx: &mpsc::Sender<Ev>,
) -> Result<(), String> {
    'stream: loop {
        // The borrow of `session` lives exactly as long as this scope, which
        // is why the pair never needs a name.
        let mut stream = session
            .stream(&config.run_options, &config.stream_options)
            .map_err(|err| format!("failed to start stream: {err}"))?;
        let mut tracker = CommitTracker::default();

        loop {
            let Ok(cmd) = cmd_rx.recv() else {
                return Ok(());
            };

            match cmd {
                Cmd::Feed(pcm) => {
                    let audio_ms = pcm.len() as u64 * 1000 / rate.max(1) as u64;
                    let started = std::time::Instant::now();

                    let update = match stream.feed(&pcm) {
                        Ok(update) => update,
                        // A cancel token fired: the element is flushing and a
                        // Restart is already on its way.
                        Err(Error::Aborted { .. }) => continue,
                        Err(err) => return Err(format!("feed failed: {err}")),
                    };

                    if report_compute(ev_tx, started, audio_ms).is_err() {
                        return Ok(());
                    }

                    // Silence never changes the committed *text*, so without this
                    // the row held back behind the last spoken word would sit
                    // there for the whole pause - and pin the timeline with it.
                    let release_settled = !update.committed_changed
                        && tracker.pending_is_settled(update.audio_committed_ms);

                    if update.committed_changed || release_settled {
                        let text = stream.text();
                        let snapshot = stream.snapshot();
                        log::debug!(
                            "commit at {}ms produced {} segment(s), {} word(s), {} token(s) at \
                             {:?} granularity",
                            update.audio_committed_ms,
                            snapshot.segments.len(),
                            snapshot.words.len(),
                            snapshot.tokens.len(),
                            snapshot.timestamp_kind,
                        );
                        let words = tracker.take_new(
                            &text.committed,
                            &commit::units(&snapshot),
                            update.audio_committed_ms,
                        );
                        if !words.is_empty()
                            && ev_tx
                                .send(Ev::Words {
                                    words,
                                    is_final: false,
                                })
                                .is_err()
                        {
                            return Ok(());
                        }
                    }

                    // Text is sparse; the timeline is not. Report how far the
                    // stream is decided so the element can close the gap even
                    // when nobody is speaking.
                    if ev_tx
                        .send(Ev::Progress {
                            committed_ms: tracker.frontier(update.audio_committed_ms),
                        })
                        .is_err()
                    {
                        return Ok(());
                    }

                    // Partials are the cheap path: no snapshot, text only.
                    if update.tentative_changed
                        && ev_tx.send(Ev::Partial(stream.text().tentative)).is_err()
                    {
                        return Ok(());
                    }
                }
                Cmd::Finalize => {
                    match stream.finalize() {
                        Ok(update) => {
                            let text = stream.text();
                            let snapshot = stream.snapshot();
                            // All the audio is consumed by now, so the drain
                            // point is what gives the last row a real duration
                            // instead of a zero-length one.
                            let drained_ms =
                                update.audio_committed_ms.max(update.input_received_ms);
                            let words = tracker.take_final(
                                &text.committed,
                                &commit::units(&snapshot),
                                drained_ms,
                            );
                            if send_final_output(ev_tx, words, drained_ms).is_err() {
                                return Ok(());
                            }
                        }
                        Err(Error::Aborted { .. }) => (),
                        Err(err) => return Err(format!("finalize failed: {err}")),
                    }
                    if ev_tx.send(Ev::Drained).is_err() {
                        return Ok(());
                    }
                    continue 'stream;
                }
                Cmd::Restart => {
                    stream.reset();
                    if ev_tx.send(Ev::Restarted).is_err() {
                        return Ok(());
                    }
                    continue 'stream;
                }
                Cmd::Stop => return Ok(()),
            }
        }
    }
}

/// Sliding-window offline inference, for families without a streaming path.
///
/// The window always begins exactly where the last emitted text ended. That
/// matters more than it looks: re-transcribing audio we have already emitted
/// text for invites the model to segment it differently the second time, and
/// then a row straddling the boundary matches neither "already emitted" nor
/// "starts after the watermark" and is silently lost. Cutting at the watermark
/// removes the ambiguity — every row a run produces is new by construction.
///
/// A row extending past the live edge is withheld, the watermark does not
/// advance, and the next run simply sees a longer window. That is
/// self-correcting: the window grows until the model finishes the phrase, so
/// it is capped to what the family will accept in one run.
struct Window {
    samples: Vec<f32>,
    /// How far this window proves nothing more will be emitted, so the element
    /// can close the timeline over silence instead of going quiet.
    frontier_ms: i64,
    /// Stream-relative time of `samples[0]`.
    start_ms: i64,
    /// End of the last row emitted; the low water mark for new text.
    emitted_up_to_ms: i64,
    pending_ms: i64,
    rate: i64,
    /// Longest window the family accepts in one run.
    max_ms: i64,
}

impl Window {
    fn samples_to_ms(&self, samples: usize) -> i64 {
        samples as i64 * 1000 / self.rate
    }

    fn ms_to_samples(&self, ms: i64) -> usize {
        (ms.max(0) * self.rate / 1000) as usize
    }

    fn push(&mut self, pcm: &[f32]) {
        self.pending_ms += self.samples_to_ms(pcm.len());
        self.samples.extend_from_slice(pcm);
    }

    fn end_ms(&self) -> i64 {
        self.start_ms + self.samples_to_ms(self.samples.len())
    }

    /// Rows from `transcript` that are new and stable enough to emit.
    fn harvest(
        &mut self,
        transcript: &Transcript,
        live_edge_offset_ms: i64,
        drain: bool,
    ) -> Vec<TimedWord> {
        let cutoff = if drain {
            i64::MAX
        } else {
            self.end_ms() - live_edge_offset_ms
        };

        let units = commit::units(transcript);

        let text = transcript.text.trim();

        if units.is_empty() && text.is_empty() {
            // The model found nothing at all in this window: silence. Advance
            // past it rather than carrying it forward, or the window grows
            // through the whole silent stretch and the next phrase comes back
            // as one row stretching from where the silence began.
            if !drain {
                self.emitted_up_to_ms = self.emitted_up_to_ms.max(cutoff);
                self.frontier_ms = self.frontier_ms.max(cutoff);
            }
            return Vec::new();
        }

        if units.is_empty() {
            // Text but no alignment (timestamps=none). Only safe to emit on
            // drain, where there is nothing left to re-transcribe and therefore
            // nothing to duplicate.
            if !drain {
                return Vec::new();
            }
            let t0_ms = self.emitted_up_to_ms.max(self.start_ms);
            let t1_ms = self.end_ms().max(t0_ms);
            self.emitted_up_to_ms = t1_ms;
            return vec![TimedWord {
                text: text.to_string(),
                t0_ms,
                t1_ms,
            }];
        }

        let mut out = Vec::new();
        let mut withheld_from_ms: Option<i64> = None;
        for unit in &units {
            let text = unit.text.trim();
            if text.is_empty() {
                // Carries nothing to publish, so it can neither be emitted nor
                // strand the timeline. Whisper returns exactly one of these for
                // every silent window, and treating it as withheld would pin
                // the frontier for as long as nobody spoke.
                continue;
            }

            let t0_ms = self.start_ms + unit.t0_ms;
            let t1_ms = (self.start_ms + unit.t1_ms).max(t0_ms);

            if t1_ms > cutoff {
                // Runs past the live edge, so it is retried against a longer
                // window next time. The timeline cannot be closed over it.
                withheld_from_ms = Some(withheld_from_ms.unwrap_or(i64::MAX).min(t0_ms));
                continue;
            }
            if t0_ms < self.emitted_up_to_ms {
                continue;
            }

            self.emitted_up_to_ms = t1_ms;
            out.push(TimedWord {
                text: text.to_string(),
                t0_ms,
                t1_ms,
            });
        }

        // Everything up to the live edge has now been considered. Only a row
        // still waiting for a longer window can be emitted below that, so the
        // timeline is closeable up to whichever comes first.
        if !drain {
            let limit = match withheld_from_ms {
                Some(t0_ms) => cutoff.min(t0_ms),
                None => cutoff,
            };
            self.frontier_ms = self.frontier_ms.max(limit);
        }

        out
    }

    /// Drop audio we have already emitted text for.
    ///
    /// If nothing was emitted this run the window keeps growing, so it is also
    /// force-trimmed to `max_ms` — the audio dropped that way is audio the
    /// model was given twice and produced nothing stable for.
    fn trim(&mut self) {
        let mut drop_ms = (self.emitted_up_to_ms - self.start_ms).max(0);

        let length_ms = self.samples_to_ms(self.samples.len());
        if length_ms - drop_ms > self.max_ms {
            let forced = length_ms - drop_ms - self.max_ms;
            log::warn!(
                "window hit the {}ms limit with no stable text; dropping {forced}ms of audio",
                self.max_ms
            );
            drop_ms += forced;
        }

        let drop = self.ms_to_samples(drop_ms).min(self.samples.len());
        if drop == 0 {
            return;
        }
        self.samples.drain(..drop);
        self.start_ms += self.samples_to_ms(drop);
        // Nothing before the window start can ever be emitted now.
        self.emitted_up_to_ms = self.emitted_up_to_ms.max(self.start_ms);
    }

    fn clear(&mut self, at_ms: i64) {
        self.samples.clear();
        self.start_ms = at_ms;
        self.emitted_up_to_ms = at_ms;
        self.pending_ms = 0;
    }
}

fn run_chunked(
    session: &mut Session,
    config: &Config,
    rate: i64,
    max_ms: i64,
    cmd_rx: &mpsc::Receiver<Cmd>,
    ev_tx: &mpsc::Sender<Ev>,
) -> Result<(), String> {
    let mut window = Window {
        samples: Vec::new(),
        start_ms: 0,
        emitted_up_to_ms: 0,
        frontier_ms: 0,
        pending_ms: 0,
        rate,
        max_ms,
    };

    loop {
        let Ok(cmd) = cmd_rx.recv() else {
            return Ok(());
        };

        match cmd {
            Cmd::Feed(pcm) => {
                window.push(&pcm);
                if window.pending_ms < config.chunk_ms {
                    continue;
                }

                // The chunk of new audio is the real-time budget for this run;
                // re-transcribing the context is overhead we chose to pay.
                let consumed_ms = window.pending_ms as u64;
                window.pending_ms = 0;
                let started = std::time::Instant::now();

                let words = match infer(session, config, &mut window, false) {
                    Ok(words) => words,
                    Err(Some(err)) => return Err(err),
                    Err(None) => continue,
                };

                if report_compute(ev_tx, started, consumed_ms).is_err() {
                    return Ok(());
                }
                if !words.is_empty()
                    && ev_tx
                        .send(Ev::Words {
                            words,
                            is_final: false,
                        })
                        .is_err()
                {
                    return Ok(());
                }
                window.trim();

                // `harvest` advances the watermark past a window even when it
                // yielded nothing, so this closes the timeline over silence the
                // same way it closes it over speech.
                if ev_tx
                    .send(Ev::Progress {
                        committed_ms: window.frontier_ms,
                    })
                    .is_err()
                {
                    return Ok(());
                }
            }
            Cmd::Finalize => {
                let words = match infer(session, config, &mut window, true) {
                    Ok(words) => words,
                    Err(Some(err)) => return Err(err),
                    Err(None) => Vec::new(),
                };
                let drained_ms = window.end_ms();
                if send_final_output(ev_tx, words, drained_ms).is_err() {
                    return Ok(());
                }
                window.clear(drained_ms);
                if ev_tx.send(Ev::Drained).is_err() {
                    return Ok(());
                }
            }
            Cmd::Restart => {
                window.clear(0);
                if ev_tx.send(Ev::Restarted).is_err() {
                    return Ok(());
                }
            }
            Cmd::Stop => return Ok(()),
        }
    }
}

/// `Err(None)` means "aborted, nothing to report" — a cancel token fired.
fn infer(
    session: &mut Session,
    config: &Config,
    window: &mut Window,
    drain: bool,
) -> Result<Vec<TimedWord>, Option<String>> {
    if window.samples.is_empty() {
        return Ok(Vec::new());
    }

    let transcript = match session.run(&window.samples, &config.run_options) {
        Ok(transcript) => transcript,
        Err(Error::Aborted { .. }) => return Err(None),
        Err(err) => return Err(Some(format!("inference failed: {err}"))),
    };

    // Which granularity the family actually produced decides whether we can
    // emit per-word buffers at all, so it is worth seeing in the log.
    log::debug!(
        "run over {}ms produced {} segment(s), {} word(s), {} token(s) at {:?} granularity",
        window.samples_to_ms(window.samples.len()),
        transcript.segments.len(),
        transcript.words.len(),
        transcript.tokens.len(),
        transcript.timestamp_kind,
    );

    Ok(window.harvest(&transcript, config.live_edge_offset_ms, drain))
}

#[cfg(test)]
mod tests {
    use super::*;
    use transcribe_cpp::{TimestampKind, Timings, Word};

    fn window(start_ms: i64, len_ms: i64) -> Window {
        let rate = 16_000;
        Window {
            samples: vec![0.0; (len_ms * rate / 1000) as usize],
            start_ms,
            emitted_up_to_ms: start_ms,
            frontier_ms: start_ms,
            pending_ms: 0,
            rate,
            max_ms: DEFAULT_MAX_WINDOW_MS,
        }
    }

    #[test]
    fn final_output_closes_silence_before_the_drain_marker() {
        let (tx, rx) = mpsc::channel();
        send_final_output(&tx, Vec::new(), 53_990).unwrap();
        tx.send(Ev::Drained).unwrap();

        assert!(matches!(
            rx.recv().unwrap(),
            Ev::Words {
                words,
                is_final: true,
            } if words.is_empty()
        ));
        assert!(matches!(
            rx.recv().unwrap(),
            Ev::Progress {
                committed_ms: 53_990,
            }
        ));
        assert!(matches!(rx.recv().unwrap(), Ev::Drained));
    }

    fn push_silence(w: &mut Window, ms: i64) {
        w.samples
            .extend(std::iter::repeat_n(0.0, (ms * w.rate / 1000) as usize));
    }

    fn transcript(words: &[(&str, i64, i64)]) -> Transcript {
        Transcript {
            text: words
                .iter()
                .map(|(t, _, _)| *t)
                .collect::<Vec<_>>()
                .join(" "),
            language: None,
            timestamp_kind: TimestampKind::Word,
            segments: Vec::new(),
            words: words
                .iter()
                .map(|(text, t0_ms, t1_ms)| Word {
                    t0_ms: *t0_ms,
                    t1_ms: *t1_ms,
                    seg_index: 0,
                    first_token: 0,
                    n_tokens: 1,
                    text: text.to_string(),
                })
                .collect(),
            tokens: Vec::new(),
            timings: Timings::default(),
        }
    }

    #[test]
    fn withholds_words_inside_the_live_edge() {
        let mut w = window(0, 4_000);
        let words = w.harvest(
            &transcript(&[("hello", 100, 500), ("world", 3_800, 3_950)]),
            1_000,
            false,
        );

        // "world" ends after the 3s stability cutoff.
        assert_eq!(
            words.iter().map(|w| w.text.as_str()).collect::<Vec<_>>(),
            ["hello"]
        );
        assert_eq!(w.emitted_up_to_ms, 500);
    }

    #[test]
    fn next_window_starts_where_the_emitted_text_ended() {
        let mut w = window(0, 4_000);
        w.harvest(&transcript(&[("hello", 100, 500)]), 1_000, false);

        // Audio we have already emitted text for is gone, so the next run
        // cannot re-segment it and lose a row across the seam.
        w.trim();
        assert_eq!(w.start_ms, 500);
        assert_eq!(w.samples_to_ms(w.samples.len()), 3_500);

        push_silence(&mut w, 4_000);
        let words = w.harvest(&transcript(&[("world", 500, 900)]), 1_000, false);
        assert_eq!(words.len(), 1);
        assert_eq!(words[0].t0_ms, 1_000);
        assert_eq!(words[0].t1_ms, 1_400);
    }

    #[test]
    fn withheld_text_is_retried_against_a_longer_window() {
        let mut w = window(0, 4_000);

        // The only row runs past the live edge, so nothing is emitted and the
        // watermark does not move.
        let words = w.harvest(&transcript(&[("hello world", 100, 3_500)]), 1_000, false);
        assert!(words.is_empty());
        w.trim();
        assert_eq!(w.start_ms, 0);
        assert_eq!(w.samples_to_ms(w.samples.len()), 4_000);

        // With more audio the row now ends well inside the window and goes out.
        push_silence(&mut w, 4_000);
        let words = w.harvest(&transcript(&[("hello world", 100, 3_500)]), 1_000, false);
        assert_eq!(words.len(), 1);
        assert_eq!(words[0].t1_ms, 3_500);
    }

    #[test]
    fn a_window_that_never_commits_is_capped() {
        let mut w = window(0, 0);
        w.max_ms = 5_000;
        push_silence(&mut w, 8_000);

        w.trim();
        assert_eq!(w.samples_to_ms(w.samples.len()), 5_000);
        assert_eq!(w.start_ms, 3_000);
        assert_eq!(w.emitted_up_to_ms, 3_000);
    }

    #[test]
    fn drain_emits_the_trailing_edge() {
        let mut w = window(0, 4_000);
        let words = w.harvest(
            &transcript(&[("hello", 100, 500), ("world", 3_800, 3_950)]),
            1_000,
            true,
        );
        assert_eq!(words.len(), 2);
    }

    #[test]
    fn silence_is_dropped_rather_than_accumulated() {
        let mut w = window(0, 4_000);

        // Nothing at all in this window: everything but the live-edge tail is
        // discarded, so the next phrase is not timestamped back into silence.
        assert!(w.harvest(&transcript(&[]), 1_000, false).is_empty());
        assert_eq!(w.emitted_up_to_ms, 3_000);

        w.trim();
        assert_eq!(w.start_ms, 3_000);
        assert_eq!(w.samples_to_ms(w.samples.len()), 1_000);
    }

    #[test]
    fn untimed_transcript_only_emits_on_drain() {
        let mut w = window(0, 4_000);
        let mut t = transcript(&[]);
        t.text = "hello world".into();

        assert!(w.harvest(&t, 1_000, false).is_empty());
        let words = w.harvest(&t, 1_000, true);
        assert_eq!(words.len(), 1);
        assert_eq!(words[0].text, "hello world");
    }
}
