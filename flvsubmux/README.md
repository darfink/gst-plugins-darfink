# gst-flvsubmux

A strict GStreamer aggregator that inserts timed text into a muxed FLV stream
as `onCaption` or `onTextData` script data. Run it after `flvmux` or `eflvmux`.
The element carries captions as one sparse subtitle timeline beside the
already-muxed audio and video:

```text
eflvmux streamable=true ! flvsubmux ! rtmp2sink
                              ^
                        text/x-raw,format=utf8
```

The plugin is `flvsubmux`. It registers one element with the same name.

## Build

Requires Rust 1.85+ and GStreamer 1.20 development headers. `eflvmux` needs
GStreamer 1.28 or newer.

```bash
cargo build --release -p gst-flvsubmux
export GST_PLUGIN_PATH="$PWD/target/release"
gst-inspect-1.0 flvsubmux
```

For permanent installation, see the workspace
[installation instructions](../README.md#install).

## Properties

| Property | Default | Meaning |
| --- | ---: | --- |
| `message-name` | `oncaption` | AMF0 message name: `oncaption` or `ontextdata` |
| `input-mode` | `timed` | Interpret input as finite `timed` intervals or persistent `replacement` states |
| `prime` | `true` | Write one empty cue at the first timestamped FLV buffer to declare the subtitle stream |

## Integration contract

The element has three always-present pads:

```text
sink   video/x-flv                  muxed stream from flvmux or eflvmux
text   text/x-raw, format=utf8      timed text or GAP coverage
src    video/x-flv                  muxed stream with script-data captions
```

Every text buffer must have a PTS and duration. A normal text buffer must be
valid UTF-8. An empty buffer is an explicit clear in `replacement` mode. GAP
events are converted into coverage intervals. The element never decodes GAP
buffers as text.

The text timeline must continuously cover the media timeline. Missing,
overlapping, non-monotonic, or contradictory coverage is an error. If the text
branch falls behind, the aggregator waits for the next buffer or GAP event and
backpressures the FLV branch. The element does not use a clock timeout, late
clamp, or silent drop.

Both sink pads must use TIME segments with a rate of `1.0`. Each timestamp must
fall inside its pad segment. The element rejects other segment formats, other
rates, and timestamps outside the segment. It does not clip or reinterpret
these values.

`input-mode=timed` shows text at its PTS and schedules a clear at PTS plus
duration. `input-mode=replacement` keeps text until another state replaces it.
An empty text buffer clears the display. In both modes, duration still
contributes to timeline coverage.

GAP buffers provide coverage only. They do not change replacement state. A
missing future GAP is a wait condition. A malformed interval is an error when
the element can prove that coverage is missing or contradictory.

The text pad must provide silence as GAP events. Text EOS can end the text
timeline when no future captions exist. Text EOS lets remaining FLV buffers
drain. FLV EOS waits for text EOS, then emits transitions within the final media
range and discards transitions beyond that range.

A new media segment resets origin calibration, priming, and caption state. A new
text segment starts a new caption timeline. If a cue was active, the next media
buffer carries an explicit clear before new caption state appears.

## Why it sits after the muxer

`flvmux` and `eflvmux` do not provide a text sink pad or a hook for arbitrary
AMF script-data messages. Their output is a sequence of self-delimiting FLV
tags with timestamps, so `flvsubmux` can insert a correctly framed caption tag
between media tags and forward the original media buffers unchanged.

The aggregator does not pace output against the clock. It only waits for the
text watermark needed to prove coverage of the next media buffer. This keeps
the element suitable for burst republishing as well as live pipelines.

After coverage is proven, a caption transition keeps its original timestamp.
The element never clamps a late transition to the current media position. It
never drops a transition because the media cadence skipped its exact boundary.

## Wire format

The caption payload is AMF0. The message name is `onCaption` by default because
it avoids a diagnostic emitted by FFmpeg for `onTextData`; both names decode to
the same text subtitle stream. The payload contains one `text` property with
the UTF-8 cue text.

Cue text longer than the AMF0 short-string limit is truncated with a warning.
The output timestamp is the FLV timestamp corresponding to the media position
at which the transition becomes ready.

## Debugging

```bash
GST_DEBUG=flvsubmux:6 gst-launch-1.0 ...
```

Logs include text coverage errors, origin calibration, queued transitions,
priming, and transitions discarded after the final media position.

## Tests

```bash
cargo clippy -p gst-flvsubmux --all-targets --all-features -- -D warnings
cargo test -p gst-flvsubmux
```

The test suite covers GAP-driven aggregation, replacement and timed input,
flush handling, EOS behavior, backpressure, timestamp continuity, and both
`flvmux` and `eflvmux` when those muxers are available in the local GStreamer
installation. It also covers segment-rate rejection, out-of-segment
timestamps, and text-segment caption clearing.

## License

MPL-2.0.
