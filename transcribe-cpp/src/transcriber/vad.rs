// SPDX-License-Identifier: MPL-2.0

//! Voice-activity gating for the head of a stream.
//!
//! Streaming model families are fed audio the moment it arrives, which makes
//! the first thing they ever see part of their state. Some of them never
//! recover from that first impression: opening a parakeet/nemotron stream on
//! silence measurably poisons it, and the speech that follows is never
//! committed. A recording that starts with 8s of room tone loses its first
//! *40 seconds* of transcript that way, and even 100ms of leading digital
//! silence is enough to drop words.
//!
//! So the audio the model sees has to *start* on speech. This gate withholds
//! buffers until [`earshot`] says a frame is voiced, and everything from that
//! point on flows straight through - the cost is bounded to the head of the
//! stream rather than being a filter in the hot path forever.
//!
//! # Timestamps survive this
//!
//! The gate never rewrites, resamples or reorders anything: it only decides
//! *when the first sample reaches the model*. The element timestamps words by
//! adding the model's stream-relative time to `base_pts`, the running time of
//! the first sample it fed - so [`Decision::Open`] reports exactly how much
//! audio was skipped, the element advances `base_pts` by that much, and the
//! model's zero and the element's zero move together. Word times still land
//! on the audio they describe.
//!
//! # It cannot stall forever
//!
//! A gate that waits for speech that never comes is a hung pipeline, so this
//! one is bounded three ways: it gives up and opens after `max_wait_ms` of
//! audio, [`Gate::take_held`] hands the withheld audio back when the stream
//! ends, and a zero bound disables it outright.

use earshot::Detector;

/// earshot consumes exactly this many samples per call, at 16 kHz.
const FRAME: usize = 256;

/// Score above which a frame counts as voice. earshot documents 0.5 as the
/// generic threshold; the gate leans slightly stricter because a false open is
/// the exact failure this mechanism exists to prevent, and a real utterance
/// produces a run of confident frames rather than one marginal one.
const DEFAULT_THRESHOLD: f32 = 0.6;

/// Consecutive voiced frames required to open. One frame is 16ms, so this is
/// ~48ms of speech: long enough to reject a click or a codec artifact, short
/// enough to sit inside the onset of the first word.
const DEFAULT_HANGOVER_FRAMES: u32 = 3;

/// How much audio to release ahead of the frame that opened the gate.
///
/// Two things want this to be non-zero. The detector needs a few frames to
/// become confident, and a plosive is loudest before it is *recognisable*, so
/// releasing exactly at the deciding frame clips the attack off the first
/// word. Less obviously, the models want it too: cutting a recording exactly
/// at the onset measurably *loses* the first utterance, while giving them a
/// few hundred milliseconds of room tone first recovers it. They appear to
/// need a moment of context before the first phoneme to normalise against.
///
/// Calibrated by sweeping 18 separate speech onsets from a real recording:
/// 400-600ms recovered the opening utterance 15 times out of 18, against 12
/// with no lead-in at all. The response is not smooth - the families are
/// erratic near the boundary, and individual onsets flip either way over tens
/// of milliseconds - so this sits in the middle of the band that measured
/// best rather than at an edge. What reliably poisons a stream is a *long*
/// opening silence, not a short one.
const PREROLL_MS: u64 = 500;

/// Amplitude below which a sample carries no signal at all.
///
/// Real capture always has a noise floor - the recording this was calibrated
/// against sits around 2e-4 between words - so anything under this is
/// synthetic: a muted encoder, a silence generator, a gap filled with zeros.
const SILENCE_EPSILON: f32 = 1e-6;

/// Least amount of dead air worth removing.
///
/// These families are erratic about where their audio starts: trimming a
/// couple of hundred milliseconds off a stream that already opens on speech
/// can cost words on its own, for no benefit, because there was no long
/// silence doing damage in the first place. Below this the gate feeds the
/// stream from its very first sample and behaves as if it were not there.
const MIN_SKIP_MS: u64 = 1_000;

/// Digital silence handed to the model before the first speech, as its own
/// chunk.
///
/// NVIDIA documents this for the Nemotron/Parakeet streaming checkpoints: a
/// stream that opens directly on speech misses its first words, and the fix
/// is a short zero-filled priming chunk ahead of the real audio. Measured
/// here against 13 clean speech onsets it recovered the opening utterance
/// 12/13 times against 11/13 with no padding, and never made one worse.
///
/// This is *not* in tension with the gate refusing to open on silence. The
/// damage comes from a long opening silence the model has to normalise
/// against; a chunk this short is a deliberate priming signal, and the
/// element controls it exactly rather than inheriting whatever the source
/// happened to send.
const WARMUP_PAD_MS: u64 = 80;

/// What [`Gate::push`] decided about a buffer.
#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    /// Still waiting for speech. The gate kept the samples.
    Withhold,
    /// Speech started. Feed `audio`, which begins `skipped_samples` after the
    /// point the gate was armed.
    ///
    /// The audio can reach back into earlier buffers, because the onset that
    /// opened the gate is usually a few frames old by the time it is certain.
    /// That is why the offset is reported against the arming point rather
    /// than against the buffer that happened to trip it.
    Open {
        skipped_samples: u64,
        audio: Vec<f32>,
        /// Zero-filled priming chunk to feed *before* `audio`, and which
        /// occupies no time on the timeline - see [`WARMUP_PAD_MS`].
        warmup: Vec<f32>,
    },
    /// The gate is already open; feed the buffer untouched.
    Passthrough,
}

/// Withholds audio until speech starts, then gets out of the way.
pub struct Gate {
    detector: Box<Detector>,
    /// Everything withheld since the gate was armed. Bounded by the wait
    /// deadline, which fires long before this becomes large: 30s of 16 kHz
    /// mono f32 is under 2 MiB, and it is freed the moment the gate opens.
    held: Vec<f32>,
    /// How much of `held` has been through the detector. The detector is
    /// frame-locked, so buffer boundaries and frame boundaries do not line up.
    processed: usize,
    voiced_run: u32,
    open: bool,
    rate: u64,
    threshold: f32,
    hangover_frames: u32,
    /// Give up and open after this much audio, so silence cannot stall the
    /// pipeline. Zero disables the gate entirely.
    max_wait_ms: u64,
    /// Priming silence fed ahead of the first speech. Zero disables it.
    warmup_pad_ms: u64,
}

impl Gate {
    pub fn new(rate: u64, max_wait_ms: u64) -> Self {
        Self {
            detector: Detector::default_boxed(),
            held: Vec::new(),
            processed: 0,
            voiced_run: 0,
            open: false,
            rate: rate.max(1),
            threshold: DEFAULT_THRESHOLD,
            hangover_frames: DEFAULT_HANGOVER_FRAMES,
            max_wait_ms,
            warmup_pad_ms: WARMUP_PAD_MS,
        }
    }

    /// Set the priming pad, in milliseconds. Zero disables it.
    pub fn set_warmup_pad_ms(&mut self, ms: u64) {
        self.warmup_pad_ms = ms;
    }

    /// The zero-filled chunk to feed before the first real audio.
    fn warmup(&self) -> Vec<f32> {
        vec![0.0; self.ms_to_samples(self.warmup_pad_ms)]
    }

    /// Re-arm for a new stream. Called wherever the worker's stream restarts,
    /// because a fresh stream is cold in exactly the way this guards against.
    pub fn reset(&mut self) {
        self.detector.reset();
        self.held = Vec::new();
        self.processed = 0;
        self.voiced_run = 0;
        self.open = false;
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Give back the withheld audio and open, for a stream that is ending.
    ///
    /// Speech the detector was not confident about is still audio the user
    /// expects a transcript for, so EOS releases it rather than dropping it on
    /// the floor. Returns `None` when nothing was held.
    pub fn take_held(&mut self) -> Option<Decision> {
        self.open = true;
        if self.held.is_empty() {
            return None;
        }
        Some(Decision::Open {
            skipped_samples: 0,
            audio: std::mem::take(&mut self.held),
            warmup: self.warmup(),
        })
    }

    fn ms_to_samples(&self, ms: u64) -> usize {
        (ms.saturating_mul(self.rate) / 1000) as usize
    }

    /// Trim leading digital silence off the preroll.
    ///
    /// The preroll exists to give the model a moment of context before the
    /// first phoneme, and real room tone does that job well. Digital zeros do
    /// the opposite: they are the very thing that poisons a cold stream, and
    /// feeding a few hundred milliseconds of them costs the first utterance
    /// even though the gate opened in the right place.
    ///
    /// So the preroll is allowed to reach back only as far as the audio is
    /// real. A file that begins with generated silence starts on its first
    /// non-zero sample; a live capture with a noise floor keeps the whole
    /// preroll.
    fn skip_digital_silence(&self, start: usize, limit: usize) -> usize {
        let head = &self.held[start..limit];
        let offset = head
            .iter()
            .position(|sample| sample.abs() > SILENCE_EPSILON)
            .unwrap_or(head.len());
        start + offset
    }

    /// Classify one buffer of mono f32 audio at the model's native rate.
    pub fn push(&mut self, samples: &[f32]) -> Decision {
        if self.open {
            return Decision::Passthrough;
        }
        // A zero bound means the caller opted out; behave like a plain wire.
        if self.max_wait_ms == 0 {
            self.open = true;
            return Decision::Passthrough;
        }

        self.held.extend_from_slice(samples);

        // Index into `held` of the first sample to feed, once something
        // decides the gate should open.
        let mut opened_at: Option<usize> = None;

        while self.processed + FRAME <= self.held.len() {
            let frame_end = self.processed + FRAME;
            let score = self
                .detector
                .predict_f32(&self.held[self.processed..frame_end]);
            self.processed = frame_end;

            if score < self.threshold {
                self.voiced_run = 0;
                continue;
            }

            self.voiced_run += 1;
            if self.voiced_run < self.hangover_frames {
                continue;
            }

            // Speech. Rewind past the frames that formed the run and the
            // preroll, so the model gets the attack of the word rather than
            // its middle.
            let rewind = self.voiced_run as usize * FRAME + self.ms_to_samples(PREROLL_MS);
            let start = frame_end.saturating_sub(rewind);
            opened_at = Some(self.skip_digital_silence(start, frame_end));
            break;
        }

        if opened_at.is_none() {
            // Bound the wait. Judged on audio rather than wall clock, so the
            // behaviour is identical for a file and a live source.
            let deadline = self.ms_to_samples(self.max_wait_ms);
            if self.held.len() >= deadline {
                // Everything before the deadline was judged silence, so the
                // model still opens on the newest audio available rather than
                // on the whole withheld stretch.
                opened_at = Some(deadline);
            }
        }

        match opened_at {
            Some(start) => {
                self.open = true;
                let held = std::mem::take(&mut self.held);
                // Removing a sliver of lead-in is all risk and no reward: the
                // damage this gate exists to prevent comes from a *long*
                // opening silence, so anything shorter is passed through
                // untouched.
                let mut start = start.min(held.len());
                if start < self.ms_to_samples(MIN_SKIP_MS) {
                    start = 0;
                }
                Decision::Open {
                    skipped_samples: start as u64,
                    audio: held[start..].to_vec(),
                    warmup: self.warmup(),
                }
            }
            None => Decision::Withhold,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u64 = 16_000;

    fn silence(ms: u64) -> Vec<f32> {
        vec![0.0; (RATE * ms / 1000) as usize]
    }

    /// Something with enough spectral structure to read as voice. A bare sine
    /// is not speech, so this stacks a glottal-ish fundamental with harmonics
    /// under a syllable-rate envelope.
    fn speech(ms: u64) -> Vec<f32> {
        let n = (RATE * ms / 1000) as usize;
        (0..n)
            .map(|i| {
                let t = i as f32 / RATE as f32;
                let envelope = 0.5 + 0.5 * (2.0 * std::f32::consts::PI * 4.0 * t).sin();
                let harmonics: f32 = (1..=12)
                    .map(|h| {
                        let amp = 1.0 / h as f32;
                        amp * (2.0 * std::f32::consts::PI * 120.0 * h as f32 * t).sin()
                    })
                    .sum();
                0.2 * envelope * harmonics
            })
            .collect()
    }

    #[test]
    fn silence_is_withheld() {
        let mut gate = Gate::new(RATE, 30_000);
        for _ in 0..10 {
            assert_eq!(gate.push(&silence(100)), Decision::Withhold);
        }
        assert!(!gate.is_open());
    }

    #[test]
    fn speech_opens_the_gate() {
        let mut gate = Gate::new(RATE, 30_000);
        // Long enough to be worth removing; a shorter lead-in is passed
        // through untouched, which has its own test.
        assert_eq!(gate.push(&silence(3_000)), Decision::Withhold);

        let decision = gate.push(&speech(500));
        let Decision::Open {
            skipped_samples,
            audio,
            ..
        } = decision
        else {
            panic!("expected the gate to open on speech, got {decision:?}");
        };
        assert!(gate.is_open());

        // The silence is gone and the speech is not: the model opens on voice.
        assert!(skipped_samples >= (RATE * 2_000 / 1000));
        assert!(!audio.is_empty());
    }

    #[test]
    fn the_onset_is_not_clipped_off_the_first_word() {
        let mut gate = Gate::new(RATE, 30_000);
        // A real noise floor, so the preroll is not trimmed as generated
        // silence - that behaviour has its own test.
        let floor: Vec<f32> = (0..(RATE as usize))
            .map(|i| if i % 2 == 0 { 2e-4 } else { -2e-4 })
            .collect();
        assert_eq!(gate.push(&floor), Decision::Withhold);

        let Decision::Open {
            skipped_samples, ..
        } = gate.push(&speech(1_000))
        else {
            panic!("expected the gate to open on speech");
        };

        // The gate needs a run of frames to be sure, so it must rewind behind
        // the deciding frame - otherwise the attack of the word is cut off.
        // Landing inside the lead-in is fine; landing late is not.
        assert!(
            skipped_samples <= (RATE * 1_000 / 1000),
            "opened {skipped_samples} samples in, past the start of speech"
        );
    }

    #[test]
    fn an_open_gate_stays_out_of_the_way() {
        let mut gate = Gate::new(RATE, 30_000);
        let _ = gate.take_held();
        assert_eq!(gate.push(&silence(100)), Decision::Passthrough);
        assert_eq!(gate.push(&speech(100)), Decision::Passthrough);
    }

    #[test]
    fn silence_cannot_stall_the_pipeline_forever() {
        let mut gate = Gate::new(RATE, 1_000);

        assert_eq!(gate.push(&silence(400)), Decision::Withhold);
        assert_eq!(gate.push(&silence(400)), Decision::Withhold);

        // Crossing the bound opens the gate even though no speech ever
        // arrived, so a silent source degrades to "transcribe everything"
        // instead of hanging.
        let decision = gate.push(&silence(400));
        assert!(
            matches!(decision, Decision::Open { .. }),
            "expected the wait bound to release the gate, got {decision:?}"
        );
        assert!(gate.is_open());
    }

    #[test]
    fn the_deadline_opens_on_the_newest_audio() {
        let mut gate = Gate::new(RATE, 1_000);
        assert_eq!(gate.push(&silence(900)), Decision::Withhold);

        let Decision::Open {
            skipped_samples,
            audio,
            ..
        } = gate.push(&silence(400))
        else {
            panic!("expected the gate to open at the deadline");
        };

        // Audio judged silent is dropped rather than replayed into the model.
        assert_eq!(skipped_samples, RATE * 1_000 / 1000);
        assert_eq!(audio.len(), (RATE * 300 / 1000) as usize);
    }

    #[test]
    fn a_zero_bound_disables_the_gate() {
        let mut gate = Gate::new(RATE, 0);
        assert_eq!(gate.push(&silence(10)), Decision::Passthrough);
        assert_eq!(gate.push(&silence(10)), Decision::Passthrough);
    }

    #[test]
    fn a_stream_that_ends_early_still_gets_its_audio() {
        let mut gate = Gate::new(RATE, 30_000);
        assert_eq!(gate.push(&silence(100)), Decision::Withhold);
        assert_eq!(gate.push(&silence(100)), Decision::Withhold);

        // EOS before the gate ever opened: the held audio is handed back
        // rather than silently dropped.
        let Some(Decision::Open {
            skipped_samples,
            audio,
            ..
        }) = gate.take_held()
        else {
            panic!("expected the held audio back");
        };
        assert_eq!(skipped_samples, 0);
        assert_eq!(audio.len(), (RATE * 200 / 1000) as usize);
        assert!(gate.is_open());
    }

    #[test]
    fn there_is_nothing_to_hand_back_once_the_gate_is_open() {
        let mut gate = Gate::new(RATE, 30_000);
        assert!(gate.take_held().is_none());
        assert_eq!(gate.push(&silence(100)), Decision::Passthrough);
        assert!(gate.take_held().is_none());
    }

    #[test]
    fn reset_re_arms_for_the_next_stream() {
        let mut gate = Gate::new(RATE, 30_000);
        let _ = gate.take_held();
        assert!(gate.is_open());

        gate.reset();
        assert!(!gate.is_open());
        assert_eq!(gate.push(&silence(100)), Decision::Withhold);
    }

    #[test]
    fn frames_are_tracked_across_buffer_boundaries() {
        // 100-sample buffers do not divide the 256-sample frame, so this only
        // works if the remainder is carried between calls.
        let mut gate = Gate::new(RATE, 30_000);
        let audio = speech(2_000);
        let opened = audio
            .chunks(100)
            .any(|chunk| matches!(gate.push(chunk), Decision::Open { .. }));
        assert!(opened, "gate never opened across fragmented buffers");
    }

    #[test]
    fn no_audio_is_lost_between_the_gate_and_the_model() {
        // Whatever the gate skips plus whatever it emits must add back up to
        // everything it was given, or the timeline the element rebuilds from
        // `skipped_samples` would not describe the audio the model saw.
        let mut gate = Gate::new(RATE, 30_000);
        let mut fed = 0u64;
        let mut skipped = 0u64;
        let mut total = 0u64;

        for chunk in silence(500)
            .chunks(1_024)
            .chain(speech(1_000).chunks(1_024))
        {
            total += chunk.len() as u64;
            match gate.push(chunk) {
                Decision::Withhold => (),
                Decision::Passthrough => fed += chunk.len() as u64,
                Decision::Open {
                    skipped_samples,
                    audio,
                    ..
                } => {
                    skipped = skipped_samples;
                    fed += audio.len() as u64;
                }
            }
        }

        assert_eq!(skipped + fed, total);
    }

    #[test]
    fn a_short_lead_in_is_passed_through_untouched() {
        // Trimming a sliver off a stream that already opens on speech can
        // cost words on its own, and there was no damaging silence to remove,
        // so the gate must hand the audio over whole.
        let mut gate = Gate::new(RATE, 30_000);
        let audio = speech(2_000);

        let Decision::Open {
            skipped_samples,
            audio: fed,
            ..
        } = gate.push(&audio)
        else {
            panic!("expected the gate to open on speech");
        };
        assert_eq!(skipped_samples, 0);
        assert_eq!(fed.len(), audio.len());
    }

    #[test]
    fn a_priming_chunk_is_offered_with_the_first_speech() {
        let mut gate = Gate::new(RATE, 30_000);
        assert_eq!(gate.push(&silence(3_000)), Decision::Withhold);

        let Decision::Open { warmup, .. } = gate.push(&speech(1_000)) else {
            panic!("expected the gate to open on speech");
        };
        assert_eq!(warmup.len(), (RATE * WARMUP_PAD_MS / 1000) as usize);
        assert!(warmup.iter().all(|s| *s == 0.0), "priming must be silent");
    }

    #[test]
    fn the_priming_chunk_can_be_turned_off() {
        let mut gate = Gate::new(RATE, 30_000);
        gate.set_warmup_pad_ms(0);
        assert_eq!(gate.push(&silence(3_000)), Decision::Withhold);

        let Decision::Open { warmup, .. } = gate.push(&speech(1_000)) else {
            panic!("expected the gate to open on speech");
        };
        assert!(warmup.is_empty());
    }

    #[test]
    fn audio_released_at_end_of_stream_is_primed_too() {
        // The model is just as cold here as it is on a normal open, so the
        // priming chunk has to come with it.
        let mut gate = Gate::new(RATE, 30_000);
        assert_eq!(gate.push(&silence(100)), Decision::Withhold);

        let Some(Decision::Open { warmup, .. }) = gate.take_held() else {
            panic!("expected the held audio back");
        };
        assert_eq!(warmup.len(), (RATE * WARMUP_PAD_MS / 1000) as usize);
    }

    #[test]
    fn generated_silence_is_kept_out_of_the_preroll() {
        // The preroll wants context, but digital zeros are the thing that
        // poisons a cold stream - so a file that opens on generated silence
        // must still start the model on real audio.
        let mut gate = Gate::new(RATE, 30_000);
        assert_eq!(gate.push(&silence(2_000)), Decision::Withhold);

        let Decision::Open { audio, .. } = gate.push(&speech(1_000)) else {
            panic!("expected the gate to open on speech");
        };
        assert!(
            audio.iter().any(|s| s.abs() > SILENCE_EPSILON),
            "the model was handed nothing but silence"
        );
        assert!(
            audio[0].abs() > SILENCE_EPSILON,
            "the model still opens on digital silence"
        );
    }

    #[test]
    fn a_real_noise_floor_survives_in_the_preroll() {
        // Real capture is never digitally silent, and that quiet lead-in is
        // exactly the context the models want, so it must not be trimmed.
        let mut gate = Gate::new(RATE, 30_000);
        let floor: Vec<f32> = (0..(RATE as usize))
            .map(|i| if i % 2 == 0 { 2e-4 } else { -2e-4 })
            .collect();
        assert_eq!(gate.push(&floor), Decision::Withhold);

        let Decision::Open { audio, .. } = gate.push(&speech(1_000)) else {
            panic!("expected the gate to open on speech");
        };
        // The preroll reached back into the noise floor rather than starting
        // at the first loud sample.
        assert!(audio.len() > (RATE * PREROLL_MS / 1000 / 2) as usize);
    }
}
