# gst-transcribe-cpp

A GStreamer speech-to-text plugin backed by [transcribe.cpp], covering every
model family that library supports (parakeet, canary, whisper, moonshine,
voxtral, qwen3-asr, nemotron, …) rather than a single one.

The element is `transcribecpptranscriber`; the plugin is `transcribecpp`.

## Build

Needs a C++ toolchain and **cmake** — `transcribe-cpp-sys` vendors transcribe.cpp
and builds it from source. On macOS, `brew install cmake`.

```bash
git clone https://github.com/darfink/gst-plugins-darfink && cd gst-plugins-darfink && cargo build --release -p gst-transcribe-cpp
```

The first build compiles ggml and takes a while. Expose the result to GStreamer
and confirm it registered:

```bash
export GST_PLUGIN_PATH="$PWD/target/release" && gst-inspect-1.0 transcribecpptranscriber
```

To install it permanently instead, use [cargo-c]:

```bash
cargo cbuild --release -p gst-transcribe-cpp
```

Backends are cargo features forwarded to `transcribe-cpp-sys`. `metal` is on by
default, so a Linux CPU build needs `--no-default-features`:

```bash
cargo build --release -p gst-transcribe-cpp --no-default-features --features cuda
```

## Try it

Grab a model — any GGUF from a family transcribe.cpp supports:

```bash
curl -LO https://huggingface.co/handy-computer/nemotron-speech-streaming-en-0.6b-gguf/resolve/main/nemotron-speech-streaming-en-0.6b-Q8_0.gguf
```

Then drop the element into a pipeline. It takes raw audio and outputs
`text/x-raw`, so `audioconvert ! audioresample` ahead of it is all the setup
there is:

```bash
gst-launch-1.0 filesrc location=speech.wav ! decodebin ! audioconvert ! audioresample ! transcribecpptranscriber model-path=nemotron-speech-streaming-en-0.6b-Q8_0.gguf ! fakesink dump=true
```

Each committed word arrives as its own timestamped buffer:

```text
00000000 (0x7ffff80032a0): 54 68 65                       The
00000000 (0x7ffff8003240): 68 75 6d 61 6e                 human
00000000 (0x7ffff8051180): 65 78 70 65 72 69 65 6e 63 65  experience.
```

Add [`textaccumulate`] to get whole sentences instead — see
[Grouping words into cues](#grouping-words-into-cues):

```bash
... ! transcribecpptranscriber model-path=model.gguf ! textaccumulate extend-duration=true ! fakesink dump=true
```

The output follows the same timed-text contract as the gst-plugins-rs
transcribers, so ecosystem formatters can be substituted directly:

```bash
# Low-latency cumulative roll-up replacement states
... ! transcribecpptranscriber model-path=model.gguf \
    ! textrollup clear-after=3000 ! flvsubinject input-mode=replacement

# Conventional finite-duration wrapped cues
... ! transcribecpptranscriber model-path=model.gguf \
    ! textwrap ! flvsubinject input-mode=timed
```

Swap `filesrc` for any live source to transcribe live; the element takes its
live path automatically when upstream answers the latency query as live.

### Docker

To try it without a local toolchain:

```bash
docker build -t gst-transcribe-cpp ./transcribe-cpp && docker run --rm gst-transcribe-cpp
```

The image builds the plugin, downloads a small model, and transcribes a
synthesised clip. It is a CPU build; see [Build](#build) for GPU backends.

## Modes

| | `stream` | `chunked` |
|---|---|---|
| Uses | `Session::stream()` — native streaming | `Session::run()` over a sliding window |
| Works with | families advertising `supports_streaming` | every family |
| Latency | the family's own commit lag | `chunk-duration` + `live-edge-offset` |
| Tuning | `commit-policy`, `stable-prefix-agreement-n` | `chunk-duration`, `live-edge-offset` |

`mode=auto` (the default) picks `stream` when the loaded model advertises
streaming support and `chunked` otherwise. The resolved mode is logged at INFO
and readable from the `model-info` property.

In `chunked` mode each window begins exactly where the last emitted text ended.
Re-transcribing audio that already produced text would let the model segment it
differently the second time, and a row straddling the seam then matches neither
"already emitted" nor "starts after the watermark" and is silently dropped —
which is precisely what a fixed overlap window does. A row that runs past the
live edge is withheld instead, and the next run sees a longer window, so a
phrase is retried until it completes. Windows are capped at the family's
`max_audio_ms` (30 s for whisper), and a window the model reports as empty is
discarded rather than carried forward, so silence does not accumulate.

## Output

The src pad carries `text/x-raw,format=utf8`, timestamped from the model's own
alignment. Every committed word buffer is valid UTF-8 with PTS and non-zero
duration; buffers are monotonic and non-overlapping. **Only committed text is
pushed** — text the model may still rewrite never reaches the pad, so the
output is append-only and safe for subtitle muxers. Initial, internal, and
trailing timeline holes become GAP events. New segments, discontinuities,
flushes, and EOS finalize or reset the append-only timeline before continuing.

Check `model-info`'s `max-timestamp-kind` (also logged at INFO as `alignment`)
before assuming a family aligns anything.

Granularity follows what the family actually produces, best first: **word** rows
where the model aligns to words, **segment** rows otherwise, and **token** rows
joined back into words as the last resort. The `timestamps` property is a
request, not a guarantee — whisper reports segments and rejects `timestamps=word`
outright with "unsupported timestamp granularity", while the streaming families
populate only tokens no matter what is asked. Leave it at `auto`.

A family may report `none` and *still* populate rows whose timestamps are all
zero — moonshine-streaming does. Those are ignored in favour of synthesizing
spans from the family's own audio progress, since trusting them would stamp the
whole transcript at the start of the stream.

In practice: whisper gives sentence-length captions, nemotron one buffer per
word, and moonshine one span per commit.

### Commit policy

`commit-policy` decides when text is allowed to become permanent. Under the
default `auto` the family commits a stable prefix as it goes; `on-finalize`
commits nothing until end of stream. On a 60 s clip the two now produce
byte-identical text, so `auto` costs nothing in quality and is the right default
— use `on-finalize` only if you specifically want a single result at EOS.

That equivalence is worth guarding, because getting it wrong is subtle. The
family commits *character* prefixes, so a word can be committed as `Ur` while
`du` is still coming. Publishing that trailing row immediately yields `Ur
poetry`, `17 century`, `extraord` — the continuation is swallowed because the
row is already marked emitted. The tracker therefore holds back exactly one row:
anything with a row after it has provably ended. It costs one word of latency.

### Grouping words into cues

A word-aligned family emits a row per word — 80 ms of text with a gap after it.
That is the right granularity for anything doing its own layout, but burned into
video at 24 fps most frames land in a gap, so the caption strobes.

Grouping is not this element's job. It emits the
`rstranscribe/final-transcript` custom downstream event that the gst-plugins-rs
transcribers use, so [`textaccumulate`] does the accumulating:

```bash
transcribecpptranscriber model-path=nemotron.gguf ! textaccumulate extend-duration=true
```

The event is pushed right after the buffer that closes an utterance — a word the
model punctuated with `.`, `!`, `?` or `…` — and again whenever the stream is
finalized (EOS, a seek, a discontinuity). `textaccumulate` drains on it by
default (`drain-on-final-transcripts`), so cues come out as whole sentences:

```
[0:00:00.010] The human experience.
[0:00:02.250] I think poetry is the compressed form of it.
[0:00:05.600] I completely agree.
[0:00:06.720] Is there a particular poet that you go back to?
```

`extend-duration=true` stretches each cue to the start of the next, which
removes the brief blink at cue boundaries — worth setting when burning captions
into video.

`rstranscribe/speaker-change` is **not** emitted: this element does not
diarize. transcribe.cpp ships Sortformer for that, but wiring it up would be a
separate element.

[`textaccumulate`]: https://gstreamer.freedesktop.org/documentation/textaccumulate/

### Partials

The volatile hypothesis never reaches the pad — `text/x-raw` has no way to
retract a buffer — so it is delivered as a signal instead, for live UI:

```rust
transcriber.connect("partial-transcript", false, |args| {
    let text = args[1].get::<String>().unwrap();
    println!("(partial) {text}");
    None
});
```

It only fires when the family actually maintains a volatile suffix. Nemotron
streaming never revises, so under `auto` its tentative text is always empty and
no partials are emitted; under `on-finalize` everything is tentative until the
end and partials carry the whole growing hypothesis. Handlers run on the
streaming thread and must not block.

Set `timestamps=none` if you do not need alignment at all; the element then
emits one buffer per commit.

## Properties

`model-path` is the only one you must set. Everything else has a default that
suits the loaded family.

| Property | Default | Meaning |
| --- | --- | --- |
| `model-path` | unset | Path to a GGUF model understood by transcribe.cpp. Required |
| `mode` | `auto` | `auto`, `stream`, or `chunked`. `auto` streams when the model supports it — see [Modes](#modes) |
| `backend` | `auto` | `auto`, `cpu`, `cpu-accel`, `metal`, `vulkan`, `cuda`. `cpu` is the deterministic choice |
| `n-threads` | 0 | CPU threads for ops that run on CPU; 0 uses the library default |
| `gpu-device` | 0 | GPU device registry index; 0 auto-selects, preferring discrete GPUs |
| `model-info` | — | **Read-only.** What the loaded model reported about itself, including `max-timestamp-kind`. Unset until loaded |

Timing. The first three add up to the latency reported downstream:

| Property | Default | Meaning |
| --- | --- | --- |
| `latency` | 1000 | Declared processing budget, in ms — see [Latency](#latency) |
| `chunk-duration` | 4000 | `mode=chunked`: new audio accumulated before each inference run, in ms |
| `live-edge-offset` | 1000 | `mode=chunked`: trailing audio whose words are withheld as unstable, in ms. Must be less than `chunk-duration` |
| `discont-threshold` | 500 | A timeline jump larger than this (ms) finalizes the current stream and starts a new one |
| `vad-max-wait` | 30000 | `mode=stream`: withhold audio until speech starts, so the model never opens its stream on silence, giving up after this many ms. 0 disables it — see [Opening on silence](#opening-on-silence) |
| `warmup-pad` | 80 | `mode=stream`: ms of digital silence fed as a priming chunk before the first speech. 0 disables it — see [Opening on silence](#opening-on-silence) |

Text and language:

| Property | Default | Meaning |
| --- | --- | --- |
| `language` | unset | Source language hint (ISO code); unset auto-detects |
| `task` | `transcribe` | `transcribe` or `translate` |
| `target-language` | unset | Target language (ISO code) when `task=translate` |
| `timestamps` | `auto` | `none`, `auto`, `segment`, `word`, `token`. A request, not a guarantee — see [Output](#output) |
| `pnc` | `default` | Punctuation and capitalization: `default`, `off`, `on`, on supporting families |
| `itn` | `default` | Inverse text normalization: `default`, `off`, `on`, on supporting families |
| `keep-special-tags` | `false` | Keep special vocabulary tags in the returned text |

Streaming and decoding:

| Property | Default | Meaning |
| --- | --- | --- |
| `commit-policy` | `auto` | `auto`, `on-finalize`, `stable-prefix` — see [Commit policy](#commit-policy) |
| `stable-prefix-agreement-n` | 0 | `mode=stream`: consecutive agreeing hypotheses before a prefix commits; 0 uses the library default |
| `n-ctx` | 0 | Decoder context cap in tokens; 0 uses the model maximum |
| `kv-type` | `auto` | K/V activation precision: `auto`, `f32`, `f16` |
| `spec-k-drafts` | -1 | Speculative-decode draft length; -1 family default, 0 disabled |
| `family-options` | unset | Family-specific knobs as a named structure — see [Examples](#examples) |

Flow control on a live source:

| Property | Default | Meaning |
| --- | --- | --- |
| `queue-size` | 32 | How many audio buffers may be queued for inference |
| `overrun` | `block` | `block` upstream until the worker catches up, or `drop` audio that does not fit the queue |

There is one signal, `partial-transcript` — see [Partials](#partials).

## Examples

Chunked, with a whisper model. transcribe.cpp reads whisper.cpp's legacy
`ggml-*.bin` files as well as GGUF, so existing whisper.cpp model directories
work as-is:

```bash
gst-launch-1.0 filesrc location=speech.wav ! wavparse ! audioconvert ! \
  audioresample ! \
  transcribecpptranscriber model-path=ggml-small.en.bin \
    chunk-duration=4000 live-edge-offset=1000 latency=1000 ! \
  fakesink dump=true
```

A genuinely live source, for testing the live path (`clocksync` alone is not
enough — the element keys off the latency query, and only a live source answers
it as live). Sender:

```bash
gst-launch-1.0 filesrc location=speech.wav ! wavparse ! audioconvert ! \
  audioresample ! audio/x-raw,rate=16000,channels=1 ! audioconvert ! \
  rtpL16pay pt=96 ! udpsink host=127.0.0.1 port=5004 sync=true
```

Receiver:

```bash
gst-launch-1.0 udpsrc port=5004 caps="application/x-rtp,media=(string)audio,clock-rate=(int)16000,encoding-name=(string)L16,channels=(int)1,payload=(int)96" ! rtpjitterbuffer latency=200 ! rtpL16depay ! audioconvert ! audioresample ! transcribecpptranscriber model-path=ggml-small.en.bin ! fakesink dump=true
```

Family-specific knobs go through one structure-valued property, named after the
family:

```bash
transcribecpptranscriber model-path=parakeet.gguf \
  family-options="parakeet-buffered,left-ms=1920,chunk-ms=640,right-ms=320"
```

Recognized names: `whisper` (run slot), `parakeet-stream`, `parakeet-buffered`,
`moonshine-streaming`, `voxtral-realtime` (stream slot). Fields are the
kebab-case spelling of the corresponding `transcribe_cpp` option field.

### Bundled example

`gst-launch-1.0` cannot connect to signals, so it cannot show partials. The
bundled example can — it prints committed buffers with their timestamps and
overwrites the volatile hypothesis in place, the way a live caption view would:

```bash
cargo run --release --example transcribe -- model.gguf speech.wav
```

It accepts `udp://PORT` in place of the file to drive the live path, and
`property=value` arguments to set any element property.

## Sample rate

There is no rate property: the element takes it from the model's own
capabilities and rejects mismatched caps. Put `audioconvert ! audioresample` in
front of it and negotiation lands on the right rate by itself. Input must be
mono `F32LE`.

## Opening on silence

Streaming families are fed audio as it arrives, so the first thing they ever
see becomes part of their state — and some of them never recover from a bad
first impression. Opening a parakeet/nemotron stream on silence measurably
poisons it: in the recording this was diagnosed against, a stream that began
with 8s of room tone committed nothing at all for the next 40 seconds of
speech, and even 100ms of leading digital silence was enough to drop words.

`mode=stream` therefore withholds audio until [earshot] reports voice, so the
model's first chunk lands on speech. `vad-max-wait` bounds the wait: a source
that is silent for that long opens the stream anyway rather than stalling the
pipeline, and a stream that ends before the gate ever opened hands its audio
over on EOS rather than dropping it. `vad-max-wait=0` disables the gate.

Timestamps are unaffected. The gate only decides *when the first sample
reaches the model*; the element anchors word times to the first sample it
actually fed, so the model's zero and the timeline's zero move together and
the transcript still aligns with the audio.

`mode=chunked` is left ungated — it re-reads its whole window every run and
does not carry the damage.

NVIDIA documents the same failure for these checkpoints, and their fix is to
send a short zero-filled chunk before the first speech. `warmup-pad` does
that: measured over 13 clean speech onsets it recovered the opening utterance
12/13 times against 11/13 unpadded, and never made one worse. The pad is
audio to the model, so the element anchors the timeline behind it by exactly
its length — the pad costs no time on the src pad.

Only the *head* of a stream is gated. Silence between utterances is left
alone: the families handle it correctly, and tearing the stream down at every
pause trades this bug for dropped words at the boundaries instead.

[earshot]: https://github.com/pykeio/earshot

## Latency

`latency` is your declared processing budget, added to the latency reported
downstream. Separately, on a live pipeline the element warns when inference
spends more wall-clock time than the audio it consumed, averaged over five
seconds of audio — that is the condition under which a live pipeline falls
behind without bound. The averaging matters: an RTP source delivers 20 ms
buffers, and per-call overhead makes any single one look catastrophic while the
stream as a whole keeps up comfortably. In `chunked` mode the
window is added on top (`latency + chunk-duration + live-edge-offset`), because
the element cannot emit a word before the window containing it is complete.

If inference cannot keep up with a live source, `overrun=block` (default)
propagates backpressure upstream and `overrun=drop` drops audio and keeps the
pipeline running at the cost of missing words.

## Performance

Adding threads buys wall-clock speed at a worse-than-linear CPU cost, so when
throughput is already comfortably ahead of real time, prefer more concurrent
streams over more threads per stream.

Prefer `stream` over `chunked` for families that support it, even offline.
Re-running an incremental model over sliding windows drops words at the window
seams: on the same clip, `chunked` turned "There was Hindi, which is the national
language" into "there was Hindi, is the national", and was slower than `stream`
because each window re-transcribes context the streaming path never revisits.

Size is a poor predictor. moonshine-streaming tiny is 48 MB against nemotron's
696 MB and is **17x slower on CPU** — its streaming loop decodes far more often
than nemotron's. `family-options="moonshine-streaming,min-decode-interval-ms=1000"`
brings it to 2x real time, still 5x nemotron's cost and at the price of the
latency that made it interesting. Measure; do not infer from parameter count.

Thread count is also not a free win: whisper tiny.en is *slower* with 8 threads
than with the library default, because the coordination costs more than the work
saved on a 39M model.

## Model sharing

Every element instance loads its own `Model` today. transcribe.cpp permits at
most one in-flight run across all sessions of a model, so sharing one model
between N elements would serialize their compute — separate loads are what buy
parallelism, at the cost of N times the memory.

## Debugging

```bash
GST_DEBUG=transcribecpptranscriber:6 gst-launch-1.0 ...
```

Logs model load (architecture, variant, backend, native rate, alignment
granularity and resolved mode at INFO), each committed word as it is pushed, and
per-run compute time against the audio consumed. On a live pipeline it warns
when inference falls behind real time.

`transcribecpplib:5` is a separate category carrying transcribe.cpp's own log
output, which is where decoder-level detail lives:

```bash
GST_DEBUG=transcribecpptranscriber:6,transcribecpplib:5 gst-launch-1.0 ...
```

## Tests

```bash
cargo test
```

Unit tests cover the commit tracker's prefix handling, token-to-word joining,
and the sliding-window logic — the parts where an off-by-one silently corrupts
text rather than failing loudly. They need no model.

## Status

Both modes are verified end to end, from files and over live RTP, on Apple
Silicon / Metal and on x86-64 / CPU.

Covered: `chunked` against whisper and `stream` against nemotron-streaming;
continuous speech, pure silence (no output, no hallucination), and
speech/silence/speech with timestamps staying aligned across the gap. Token rows
are joined into words, so buffer durations track word length. Incremental output
is byte-identical to both the raw committed deltas and to
`commit-policy=on-finalize`, so neither the token joining nor the incremental
commit loses anything.

Untested: flush/seek, `overrun=drop` under genuine overload, and translation.

Deliberately not implemented: translation request pads, diarization, a shared
model cache.

## License

MPL-2.0.

[transcribe.cpp]: https://github.com/handy-computer/transcribe.cpp
[cargo-c]: https://github.com/lu-zero/cargo-c
