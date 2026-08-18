# gst-flvsubinject

A GStreamer element that injects timed text into a muxed FLV stream as
`onCaption` / `onTextData` script data, so captions travel over RTMP as one
sparse subtitle timeline instead of being embedded in every video rendition.

```text
flvmux ! flvsubinject ! rtmp2sink
           ^
     text/x-raw,format=utf8
```

The element is `flvsubinject`; the plugin is `flvsubinject`.

## Why it sits after the muxer

`flvmux` and `eflvmux` declare exactly two sink pad templates, `video` and
`audio`, and write exactly one script-data message, `onMetaData`. There is no
text pad to request and no hook for arbitrary AMF messages, in any GStreamer
release through 1.28.5.

Their output, however, is a flat sequence of self-delimiting tags, each stamped
with a rebased millisecond timestamp. A correctly framed script-data tag can
simply be spliced between two existing tags, which is all this element does.

More precisely, the muxers emit **exactly one whole FLV tag per buffer**, with
the running time of the media it carries in `GST_BUFFER_PTS`. So this element
does not parse the byte stream at all: it reads the timestamp the muxer already
computed and forwards the buffer untouched.

That invariant is asserted directly against **both** `flvmux` and `eflvmux`
(`every_muxer_buffer_is_exactly_one_tag`), with audio and video interleaved and
through a `queue`, since those are the conditions under which buffers would
plausibly be merged. It is also checked at runtime: a buffer that is not one
whole tag warns rather than silently misplacing cues, on the first violation
and periodically after that, so a continuous fault stays visible without
flooding the log.

Sitting after the muxer has a second consequence that matters more than the
convenience: **the text path never enters an aggregator.** `cccombiner` and
`matroskamux` block until every sink pad is non-empty, which is what forces
keepalive machinery onto sparse caption branches. This element waits for
nothing, and silence costs zero bytes.

## Why not a `GstAggregator`

An aggregator would supply pad-alignment for free, which is how `cccombiner`
gets its guarantee. It does not fit here, and the reason is worth stating
because it looks like the obvious choice.

`cccombiner` never sets `force-live`. It does not need to: `tttocea708` feeds
it one caption buffer per video frame, padding included, so its caption pad is
never empty. A `GstAggregator` that *does* see an empty pad waits for it
indefinitely unless it is live — the timeout branch in
`gst_aggregator_wait_and_check()` is reached only when latency is valid.

A script-data caption timeline is sparse by design; that sparseness is the
bitrate saving. Satisfying an aggregator would mean padding the text pad back
to frame rate, adding keepalive GAP events, or running live and accepting
clock-paced output — and the last one caps throughput at 1×, which a burst
republish path cannot afford.

So alignment here is built rather than inherited: the FLV thread is the only
writer, cues are placed against the tag timestamps they precede, and both
properties are covered by `tests/ordering.rs`.

## Wire format

The layout is dictated by FFmpeg's `flv_data_packet()` in
`libavformat/flvdec.c`, which is the reader every consumer ultimately goes
through:

* the message name must be `onTextData`, `onCaption`, or `onCaptionInfo`;
* the payload must be an ECMA mixed array, object, or strict array;
* the cue text lives in a property named `text` holding an AMF0 string.

Two details of that reader constrain what is written. Property names are read
into a `char buf[20]`, and FFmpeg stops at the *first* `text` property it
finds, so exactly one is written, first.

`onCaption` is the default rather than the more familiar `onTextData`, because
`onTextData` reaches `avpriv_request_sample()` first and logs
`OnTextData packet is not implemented` once per cue before decoding it
correctly. Both reach the same branch and produce an `AV_CODEC_ID_TEXT`
subtitle stream.

## Input modes and FLV replacement semantics

FFmpeg sets `pkt->dts = pkt->pts = dts` and no packet duration. The FLV wire
model is therefore strictly **state replacement**: text displays until another
script-data state arrives.

`input-mode=timed` (the default) adapts ordinary timed-text sources such as
`whispertranscriber ! textwrap`: a non-empty buffer shows at PTS and schedules
a generation-bound clear at `PTS + duration`. An overlapping newer cue cannot
be erased by the older cue's clear. Adjacent transitions at one timestamp are
coalesced, so replacement never exposes a transient blank.

`input-mode=replacement` accepts persistent display states such as
`textrollup`: non-empty text replaces the state at PTS, duration is validated
but does not clear it, and empty text explicitly clears it. Identical states
are suppressed in both modes.

## Priming

FLV has no track table. A script-data subtitle stream exists only once its
first cue has been seen, and a demuxer that finishes probing before then
concludes the stream has no captions and never revisits it.

Speech-derived captions always lose that race: the first cue cannot appear
until someone has spoken and the recognizer has committed a word, which is
seconds after any reasonable probe window closes.

`prime` (default `true`) writes one empty state at the head of the stream to
declare the timeline. It is the exact analogue of what CEA-708 does by sending
null padding from the first frame, long before any caption text exists. Empty
text is also the explicit clear representation, so a stateful consumer remains
blank rather than tracking an invisible active cue until speech begins.

## Properties

| Property | Default | Meaning |
| --- | ---: | --- |
| `message-name` | `oncaption` | AMF0 message name: `oncaption` or `ontextdata` |
| `late-policy` | `clamp` | A cue the stream has passed: `clamp` to the current position, or `drop` |
| `input-mode` | `timed` | Interpret input as `timed` intervals or persistent `replacement` states |
| `prime` | `true` | Declare the subtitle stream with one invisible cue at the start |

## Integration contract

Three always pads:

```text
sink   video/x-flv      the muxed stream, from flvmux or eflvmux
text   text/x-raw, format=utf8
src    video/x-flv
```

Every text buffer must carry PTS and duration; malformed input fails the text
flow rather than being silently reinterpreted. Cue text must be valid UTF-8,
and is truncated at 64 KB with a warning — the AMF0 short-string limit.

Both sink pads are compared in **running time**, which is the only domain the
two branches share. The element calibrates its cue origin from the first
timestamped FLV buffer, because the muxer rebases tag timestamps against its
own first sample: keying off the first buffer of any kind would leave the
origin unset, since stream headers carry no PTS.

The FLV thread is the only writer. The text pad's events do not reach the
source pad — forwarding a text `caps` or `EOS` would renegotiate the output or
end the stream while A/V is still flowing.

On EOS, transitions at or before final media position are applied; future
transitions are discarded rather than clamped onto media that never existed.
On `FlushStop`, queued transitions and calibrated origin are dropped because
they belong to a timeline that is no longer being sent.

## Debugging

```bash
GST_DEBUG=flvsubinject:6 gst-launch-1.0 ...
```

Logs include origin calibration, each queued transition, priming, and a
teardown summary of shows, clears, identical suppression, late clamp/drop, and
future discards. A buffer that is
not exactly one FLV tag warns on the first occurrence and periodically after.

## What this element does not do

Deliberately narrow. It does not wrap text, decide when a caption is complete,
clear the display on silence, or know that speech recognition exists. Those are
windowing decisions belonging to the element producing the cues, for the same
reason `tttocea708` owns roll-up state while `h264ccinserter` owns only
carriage.

In particular, a cue whose text is empty is **not** filtered out: a caller that
wants to signal "clear the display" must be able to, and suppressing it would
strand the previous caption on screen.

## Clearing the display

FLV script data has no explicit erase. CEA-708 has one — `tttocea708` responds
to a GAP event by generating an empty window, and enforces a `roll-up-timeout`
itself — so one element owns both the decision and its enforcement.

The same model is available here, expressed in the only vocabulary this
transport has: **an empty cue means "stop displaying"**.

* `textrollup` decides when to clear (`clear-after`) and emits a zero-duration
  empty state at that media position.
* This element forwards it like any other cue.
* A consumer reads an empty cue as an instruction: it ends whatever is open and
  publishes nothing in its place.

That keeps the decision and its enforcement with the element that made it. It
matters because a cue with no end otherwise runs until its successor or until a
cap the *consumer* chose: measured with a publisher clear at 10s against a 3s
consumer cap, captions ended at exactly `start + 3s`, ignoring the publisher
entirely.

A consumer that does not implement this may treat an empty cue as an empty
caption or reject it outright. Consumers that support the replacement-state
contract must treat it as an explicit blank state.

## Build

```bash
cargo build --release -p gst-flvsubinject
export GST_PLUGIN_PATH="$PWD/target/release"
gst-inspect-1.0 flvsubinject
```

Requires Rust 1.85+ and GStreamer 1.20 development headers. `eflvmux` needs
GStreamer 1.28 or newer; the tests skip it with a notice on older builds.

## Try it

Mux a test pattern, inject two cues, and read them back with FFmpeg:

```bash
gst-launch-1.0 -e \
  videotestsrc num-buffers=90 ! x264enc speed-preset=ultrafast ! h264parse \
  ! eflvmux streamable=true ! flvsubinject name=inject ! filesink location=out.flv \
  appsrc name=cues format=time caps=text/x-raw,format=utf8 ! inject.text
```

In a real pipeline the text pad is fed by a caption source such as
[gst-textrollup]. To confirm the result is readable by the ecosystem:

```bash
ffprobe -select_streams s -show_streams out.flv     # Subtitle: text
ffmpeg -i out.flv -map 0:s:0 -f srt -               # the cues themselves
```

## Tests

```bash
cargo test
```

Unit tests assert the byte layout against the specification. The round-trip
tests in `tests/roundtrip.rs` assert it against the reader that matters, by
muxing a real stream and requiring `ffmpeg` to demux the cues back at their
original timestamps.

Both the round-trip and ordering suites run against `flvmux` **and** `eflvmux`.
Production publishes Enhanced FLV, so covering only the classic muxer would
leave the one that actually runs untested; they are separate implementations
(`gstflvmux.c` and `gsteflvmux.c`) and this element depends on a property of
their output. A muxer missing from the local GStreamer build is skipped with a
printed notice rather than passing silently.

### A pre-existing hazard the tests filter

`flvmux` rewrites `onMetaData` whenever a pad's codec info or tags change, so a
live stream carries several identical `onMetaData` tags at non-zero timestamps.
FFmpeg treats every `FLV_TAG_TYPE_META` as `FLV_STREAM_TYPE_SUBTITLE` and only
skips a metadata tag when `dts == 0`, so each repeat surfaces as a spurious
subtitle packet holding a lone AMF control byte.

This predates this element and reproduces with a bare `flvmux ! filesink`. The
tests filter exactly that shape rather than asserting it away.

## Status and limitations

In production use behind `eflvmux` for live transcription captions, with the
wire format verified against FFmpeg 8 by round-trip rather than by reading the
specification.

Known limits:

* **Flush and seek are covered synthetically only.** The harness tests drive
  `FlushStop` directly; no test exercises a flush from a live RTMP source,
  because the live path never seeks.
* **Cue durations are advisory.** A `duration` property is written when asked
  for, but FFmpeg ignores it and resolves a cue's end from its successor, so
  the presentation model is cue replacement whatever the value says.
* **One text pad.** Multiple languages would need multiple message names or a
  language property, neither of which the FLV convention specifies.

## License

MPL-2.0.

[gst-textrollup]: https://github.com/darfink/gst-textrollup
