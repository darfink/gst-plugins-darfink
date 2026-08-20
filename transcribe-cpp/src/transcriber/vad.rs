// SPDX-License-Identifier: MPL-2.0

//! Voice-activity gating for the head of a stream.
//!
//! Streaming model families are fed audio the moment it arrives, which makes
//! the first thing they ever see part of their state. Some of them never
//! recover from that first impression: opening a parakeet/nemotron stream on
//! silence measurably poisons it, and the speech that follows is never
//! committed. A recording that starts with 8s of room tone loses its first
//! *40 seconds* of transcript that way, and one that opens with 106s of
//! digital silence loses the opening sentence.
//!
//! So the audio the model sees has to *start* on speech. This gate discards
//! everything until [`earshot`] says a frame is voiced, and everything from
//! that point on flows straight through - the cost is bounded to the head of
//! the stream rather than being a filter in the hot path forever.
//!
//! # Discarded, not buffered
//!
//! Audio the detector rejects is dropped as soon as it has been classified.
//! Only [`PREROLL_MS`] of it is retained, in a ring buffer, so the attack of
//! the first word is not clipped off. That keeps memory flat no matter how
//! long the wait - an hour of silence costs the same as a second - and means
//! silence never reaches the model at all, which is where the compute goes.
//!
//! # Timestamps survive this
//!
//! The gate never rewrites, resamples or reorders anything: it only decides
//! *which sample is the first the model sees*. [`Decision::Open`] reports
//! that sample's position as `skipped_samples`, counted from the moment the
//! gate was armed, so the element can place the model's zero on the timeline
//! exactly. Dropping the silence in front shifts the model's clock and the
//! element's clock by the same amount, and word times still land on the audio
//! they describe.
//!
//! # It cannot stall
//!
//! Withholding audio never blocks: every buffer is classified and returned
//! immediately, and the element keeps publishing gap events so a downstream
//! aggregator can advance. There is deliberately no deadline - a timer that
//! fires part-way through a long lead-in hands the model exactly the silence
//! the gate exists to keep out, which is worse than waiting. If the detector
//! is wrong for a given source, turn the gate off rather than time it out.

use earshot::Detector;

/// earshot consumes exactly this many samples per call, at 16 kHz.
const FRAME: usize = 256;

/// Score above which a frame counts as voice.
///
/// earshot documents 0.5 as the generic threshold; the gate leans slightly
/// stricter because a false open is the exact failure it exists to prevent,
/// and a real utterance produces a run of confident frames rather than one
/// marginal one.
///
/// The separation is wide enough that the exact value barely matters. Across
/// two real recordings, voiced frames sit at a median of 0.91 and rejected
/// frames at 0.21, and anything from 0.6 to 0.8 opens the gate within ~100ms
/// of the true onset. Below 0.5 it starts tripping on room noise: 0.3 opened
/// four seconds early on one of them.
pub const DEFAULT_THRESHOLD: f32 = 0.6;

/// Consecutive voiced frames required to open. One frame is 16ms, so this is
/// ~48ms of speech: long enough to reject a click or a codec artifact, short
/// enough to sit inside the onset of the first word.
const DEFAULT_HANGOVER_FRAMES: u32 = 3;

/// How much audio to keep behind the frame that opens the gate.
///
/// Two things want this to be non-zero. The detector needs a few frames to
/// become confident, and a plosive is loudest before it is *recognisable*, so
/// releasing exactly at the deciding frame clips the attack off the first
/// word. Less obviously, the models want it too: cutting a recording exactly
/// at the onset measurably *loses* the first utterance, while giving them a
/// few hundred milliseconds of lead-in recovers it.
///
/// Calibrated by sweeping 18 separate speech onsets from a real recording:
/// 400-600ms recovered the opening utterance 15 times out of 18, against 12
/// with no lead-in at all. This also sizes the ring buffer, so it is the only
/// audio the gate ever retains.
const PREROLL_MS: u64 = 500;

/// Amplitude below which a sample carries no signal at all.
///
/// Real capture always has a noise floor - the recording this was calibrated
/// against sits around 2e-4 between words - so anything under this is
/// synthetic: a muted encoder, a silence generator, a gap filled with zeros.
const SILENCE_EPSILON: f32 = 1e-6;

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
    /// No speech yet. The samples were classified and discarded.
    Withhold,
    /// Speech started. Feed `audio`, whose first sample sits
    /// `skipped_samples` after the point the gate was armed.
    ///
    /// The audio reaches back into buffers already seen, because the onset
    /// that opened the gate is a few frames old by the time it is certain.
    /// That is why the offset is reported against the arming point rather
    /// than against the buffer that happened to trip it - it is what lets the
    /// element place the model's zero on the timeline.
    Open {
        skipped_samples: u64,
        audio: Vec<f32>,
        /// Zero-filled priming chunk to feed *before* `audio`, which occupies
        /// no time on the timeline - see [`WARMUP_PAD_MS`].
        warmup: Vec<f32>,
    },
    /// The gate is already open; feed the buffer untouched.
    Passthrough,
}

/// Withholds audio until speech starts, then gets out of the way.
pub struct Gate {
    detector: Box<Detector>,
    /// The most recent [`PREROLL_MS`] of rejected audio, oldest first.
    ///
    /// This is the only audio the gate retains. Everything older has been
    /// classified as silence and dropped, so memory is flat however long the
    /// wait lasts.
    preroll: std::collections::VecDeque<f32>,
    /// Samples not yet classified, always shorter than one frame. The detector
    /// is frame-locked and buffer boundaries do not line up with frames.
    pending: Vec<f32>,
    /// Total samples seen since the gate was armed. The timeline is rebuilt
    /// from this, so it counts discarded audio too.
    seen: u64,
    voiced_run: u32,
    open: bool,
    rate: u64,
    threshold: f32,
    hangover_frames: u32,
    /// Priming silence fed ahead of the first speech. Zero disables it.
    warmup_pad_ms: u64,
}

impl Gate {
    pub fn new(rate: u64) -> Self {
        Self {
            detector: Detector::default_boxed(),
            preroll: std::collections::VecDeque::new(),
            pending: Vec::with_capacity(FRAME),
            seen: 0,
            voiced_run: 0,
            open: false,
            rate: rate.max(1),
            threshold: DEFAULT_THRESHOLD,
            hangover_frames: DEFAULT_HANGOVER_FRAMES,
            warmup_pad_ms: WARMUP_PAD_MS,
        }
    }

    /// Set the score a frame must reach to count as voice.
    pub fn set_threshold(&mut self, threshold: f32) {
        self.threshold = threshold;
    }

    /// Set the priming pad, in milliseconds. Zero disables it.
    pub fn set_warmup_pad_ms(&mut self, ms: u64) {
        self.warmup_pad_ms = ms;
    }

    /// Re-arm for a new stream. Called wherever the worker's stream restarts,
    /// because a fresh stream is cold in exactly the way this guards against.
    pub fn reset(&mut self) {
        self.detector.reset();
        self.preroll = std::collections::VecDeque::new();
        self.pending.clear();
        self.seen = 0;
        self.voiced_run = 0;
        self.open = false;
    }

    #[cfg(test)]
    pub fn is_open(&self) -> bool {
        self.open
    }

    fn ms_to_samples(&self, ms: u64) -> usize {
        (ms.saturating_mul(self.rate) / 1000) as usize
    }

    /// The zero-filled chunk to feed before the first real audio.
    fn warmup(&self) -> Vec<f32> {
        vec![0.0; self.ms_to_samples(self.warmup_pad_ms)]
    }

    /// Keep a frame of rejected audio, dropping whatever no longer fits in
    /// the preroll window.
    fn retain(&mut self, frame: &[f32]) {
        let capacity = self.ms_to_samples(PREROLL_MS);
        if capacity == 0 {
            return;
        }
        self.preroll.extend(frame.iter().copied());
        while self.preroll.len() > capacity {
            self.preroll.pop_front();
        }
    }

    /// Build the audio to open on: the retained preroll, then `tail`.
    ///
    /// Leading digital silence is trimmed out of the preroll. The preroll
    /// exists to give the model context before the first phoneme, and real
    /// room tone does that job - but digital zeros are the very thing that
    /// poisons a cold stream, so they are never worth carrying in.
    fn open_on(&mut self, tail: &[f32]) -> Decision {
        let mut audio: Vec<f32> = self.preroll.iter().copied().collect();
        let trimmed = audio
            .iter()
            .position(|sample| sample.abs() > SILENCE_EPSILON)
            .unwrap_or(audio.len());
        audio.drain(..trimmed);
        audio.extend_from_slice(tail);

        self.open = true;
        self.preroll = std::collections::VecDeque::new();
        self.pending.clear();

        // Everything seen but not handed over was dropped, and the element
        // rebuilds the timeline from exactly that count.
        let skipped_samples = self.seen.saturating_sub(audio.len() as u64);
        Decision::Open {
            skipped_samples,
            audio,
            warmup: self.warmup(),
        }
    }

    /// Classify one buffer of mono f32 audio at the model's native rate.
    pub fn push(&mut self, samples: &[f32]) -> Decision {
        if self.open {
            return Decision::Passthrough;
        }

        self.seen += samples.len() as u64;

        let mut cursor = 0usize;
        while cursor < samples.len() {
            let want = FRAME - self.pending.len();
            let take = want.min(samples.len() - cursor);
            self.pending
                .extend_from_slice(&samples[cursor..cursor + take]);
            cursor += take;

            if self.pending.len() < FRAME {
                break;
            }

            let frame = std::mem::take(&mut self.pending);
            self.pending = Vec::with_capacity(FRAME);
            let score = self.detector.predict_f32(&frame);

            if score < self.threshold {
                self.voiced_run = 0;
                self.retain(&frame);
                continue;
            }

            self.voiced_run += 1;
            // Whether or not the run is long enough yet, this frame may be
            // carrying the onset, so it belongs in the preroll either way.
            self.retain(&frame);
            if self.voiced_run < self.hangover_frames {
                continue;
            }

            // Speech. The frames that formed the run are already retained, so
            // opening on the ring buffer plus the rest of this buffer keeps
            // the attack of the word intact.
            let tail = samples[cursor..].to_vec();
            return self.open_on(&tail);
        }

        Decision::Withhold
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

    /// A quiet but non-zero noise floor, the way real capture sounds between
    /// words.
    fn room_tone(ms: u64) -> Vec<f32> {
        (0..(RATE * ms / 1000) as usize)
            .map(|i| if i % 2 == 0 { 2e-4 } else { -2e-4 })
            .collect()
    }

    #[test]
    fn silence_is_withheld() {
        let mut gate = Gate::new(RATE);
        for _ in 0..10 {
            assert_eq!(gate.push(&silence(100)), Decision::Withhold);
        }
        assert!(!gate.is_open());
    }

    #[test]
    fn speech_opens_the_gate() {
        let mut gate = Gate::new(RATE);
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
        assert!(!audio.is_empty());
        assert!(skipped_samples >= (RATE * 2_000 / 1000));
    }

    #[test]
    fn silence_is_discarded_rather_than_buffered() {
        // The whole point of the ring buffer: an arbitrarily long wait costs
        // a fixed amount of memory, so a muted source cannot grow without
        // bound.
        let mut gate = Gate::new(RATE);
        for _ in 0..600 {
            assert_eq!(gate.push(&silence(100)), Decision::Withhold);
        }

        let capacity = (RATE * PREROLL_MS / 1000) as usize;
        assert!(
            gate.preroll.len() <= capacity,
            "retained {} samples, more than the {capacity}-sample preroll",
            gate.preroll.len()
        );
    }

    #[test]
    fn the_model_never_sees_the_discarded_silence() {
        let mut gate = Gate::new(RATE);
        for _ in 0..100 {
            assert_eq!(gate.push(&room_tone(100)), Decision::Withhold);
        }

        let Decision::Open { audio, .. } = gate.push(&speech(1_000)) else {
            panic!("expected the gate to open on speech");
        };
        let ceiling = (RATE * (PREROLL_MS + 1_000) / 1000) as usize;
        assert!(
            audio.len() <= ceiling,
            "handed the model {} samples, more than preroll + buffer",
            audio.len()
        );
    }

    #[test]
    fn the_timeline_accounts_for_every_sample() {
        // The element rebuilds the model's zero from `skipped_samples`, so
        // dropped + delivered has to equal everything the gate was given. If
        // this drifts, every word lands at the wrong time.
        let mut gate = Gate::new(RATE);
        let mut total = 0u64;

        for chunk in room_tone(4_000).chunks(1_024) {
            total += chunk.len() as u64;
            assert_eq!(gate.push(chunk), Decision::Withhold);
        }

        let voice = speech(1_000);
        let mut opened = None;
        for chunk in voice.chunks(1_024) {
            total += chunk.len() as u64;
            if let Decision::Open {
                skipped_samples,
                audio,
                ..
            } = gate.push(chunk)
            {
                opened = Some((skipped_samples, audio.len() as u64));
                break;
            }
        }

        let (skipped, delivered) = opened.expect("gate never opened");
        assert_eq!(skipped + delivered, total);
    }

    #[test]
    fn the_onset_is_not_clipped_off_the_first_word() {
        let mut gate = Gate::new(RATE);
        assert_eq!(gate.push(&room_tone(2_000)), Decision::Withhold);

        let Decision::Open { audio, .. } = gate.push(&speech(1_000)) else {
            panic!("expected the gate to open on speech");
        };
        assert!(audio.len() > (RATE * 1_000 / 1000) as usize);
    }

    #[test]
    fn generated_silence_is_kept_out_of_the_preroll() {
        // Digital zeros are what poisons a cold stream, so they must never be
        // carried in as context even though the preroll window covers them.
        let mut gate = Gate::new(RATE);
        assert_eq!(gate.push(&silence(3_000)), Decision::Withhold);

        let Decision::Open { audio, .. } = gate.push(&speech(1_000)) else {
            panic!("expected the gate to open on speech");
        };
        assert!(
            audio[0].abs() > SILENCE_EPSILON,
            "the model still opens on digital silence"
        );
    }

    #[test]
    fn a_real_noise_floor_survives_in_the_preroll() {
        let mut gate = Gate::new(RATE);
        assert_eq!(gate.push(&room_tone(2_000)), Decision::Withhold);

        let Decision::Open { audio, .. } = gate.push(&speech(500)) else {
            panic!("expected the gate to open on speech");
        };
        assert!(audio.len() > (RATE * 500 / 1000) as usize);
    }

    #[test]
    fn an_open_gate_stays_out_of_the_way() {
        let mut gate = Gate::new(RATE);
        let _ = gate.push(&speech(2_000));
        assert!(gate.is_open());
        assert_eq!(gate.push(&silence(100)), Decision::Passthrough);
        assert_eq!(gate.push(&speech(100)), Decision::Passthrough);
    }

    #[test]
    fn a_quiet_source_waits_indefinitely_at_no_cost() {
        // There is no deadline: a deadline fires part-way through a long
        // lead-in and hands the model the silence the gate exists to keep
        // out. Waiting is free, so the gate simply keeps waiting.
        let mut gate = Gate::new(RATE);
        for _ in 0..600 {
            assert_eq!(gate.push(&room_tone(100)), Decision::Withhold);
        }
        assert!(!gate.is_open());
        assert!(gate.preroll.len() <= (RATE * PREROLL_MS / 1000) as usize);
    }

    #[test]
    fn a_stricter_threshold_still_opens_on_speech() {
        let mut gate = Gate::new(RATE);
        gate.set_threshold(0.8);
        assert_eq!(gate.push(&room_tone(1_000)), Decision::Withhold);

        let decision = gate.push(&speech(1_000));
        assert!(
            matches!(decision, Decision::Open { .. }),
            "expected speech to clear a strict threshold, got {decision:?}"
        );
    }

    #[test]
    fn an_impossible_threshold_never_opens() {
        // The knob genuinely gates: at a threshold nothing can reach, the
        // model is never handed anything.
        let mut gate = Gate::new(RATE);
        gate.set_threshold(1.1);
        assert_eq!(gate.push(&speech(2_000)), Decision::Withhold);
        assert!(!gate.is_open());
    }

    #[test]
    fn reset_re_arms_for_the_next_stream() {
        let mut gate = Gate::new(RATE);
        let _ = gate.push(&speech(2_000));
        assert!(gate.is_open());

        gate.reset();
        assert!(!gate.is_open());
        assert_eq!(gate.push(&silence(100)), Decision::Withhold);
    }

    #[test]
    fn frames_are_tracked_across_buffer_boundaries() {
        // 100-sample buffers do not divide the 256-sample frame, so this only
        // works if the remainder is carried between calls.
        let mut gate = Gate::new(RATE);
        let audio = speech(2_000);
        let opened = audio
            .chunks(100)
            .any(|chunk| matches!(gate.push(chunk), Decision::Open { .. }));
        assert!(opened, "gate never opened across fragmented buffers");
    }

    #[test]
    fn a_priming_chunk_is_offered_with_the_first_speech() {
        let mut gate = Gate::new(RATE);
        assert_eq!(gate.push(&silence(3_000)), Decision::Withhold);

        let Decision::Open { warmup, .. } = gate.push(&speech(1_000)) else {
            panic!("expected the gate to open on speech");
        };
        assert_eq!(warmup.len(), (RATE * WARMUP_PAD_MS / 1000) as usize);
        assert!(warmup.iter().all(|s| *s == 0.0), "priming must be silent");
    }

    #[test]
    fn the_priming_chunk_can_be_turned_off() {
        let mut gate = Gate::new(RATE);
        gate.set_warmup_pad_ms(0);
        assert_eq!(gate.push(&silence(3_000)), Decision::Withhold);

        let Decision::Open { warmup, .. } = gate.push(&speech(1_000)) else {
            panic!("expected the gate to open on speech");
        };
        assert!(warmup.is_empty());
    }
}
