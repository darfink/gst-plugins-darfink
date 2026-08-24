# gst-scuffle-rtmp

A GStreamer source element that accepts one RTMP publisher and outputs an FLV
byte stream, built on [scuffle-rtmp].

```text
scufflertmplistensrc ! flvdemux
```

Unlike `rtmpsrc`, this element *listens* rather than connects: it binds a port,
waits for a publisher to arrive, and hands you the stream. That makes it useful
as an ingest endpoint rather than a client. Enhanced RTMP is supported,
including publishers sending [multiple video tracks](#enhanced-rtmp-and-multitrack)
over a single connection.

## Build

Requires Rust 1.92+ and GStreamer 1.28 development headers.

```bash
git clone https://github.com/darfink/gst-plugins-darfink && cd gst-plugins-darfink && cargo build --release -p gst-scuffle-rtmp
```

Expose the built plugin to GStreamer and confirm it registered:

```bash
export GST_PLUGIN_PATH="$PWD/target/release" && gst-inspect-1.0 scufflertmplistensrc
```

To install it permanently, use `cargo-c` and follow the workspace
[installation instructions](../README.md#install).

## Try it

Start the receiving pipeline first — the element binds when it starts, and the
publisher needs somewhere to connect:

```bash
gst-launch-1.0 -e scufflertmplistensrc address=127.0.0.1 port=1935 ! flvdemux name=demux demux.video ! queue ! h264parse ! fakesink sync=false demux.audio ! queue ! aacparse ! fakesink sync=false
```

Then publish to it:

```bash
ffmpeg -re -i input.mp4 -t 10 -map 0:v:0 -map 0:a:0 -c copy -f flv rtmp://127.0.0.1:1935/live/test
```

### Docker

If you would rather not install GStreamer locally:

```bash
docker build -t gst-scuffle-rtmp -f scuffle-rtmp/Dockerfile . &&
  docker run --rm -p 1935:1935 gst-scuffle-rtmp
```

That runs the listener pipeline on port 1935; publish to
`rtmp://127.0.0.1:1935/live/test` from the host.

## Properties

| Property | Default | Meaning |
| --- | --- | --- |
| `address`, `port` | `0.0.0.0`, `1935` | Listen endpoint |
| `application`, `stream-key` | unset | Require exact matches for the two path components in `rtmp://host/application/stream-key`. A mismatch closes that connection; it fails the pipeline unless `keep-listening=true` |
| `tcp-nodelay` | `true` | `TCP_NODELAY` on the accepted connection |
| `accept-timeout` | 0 | Nanoseconds to wait for a publisher; 0 waits indefinitely |
| `handshake-timeout` | 10s | Per-handshake-read timeout in nanoseconds; 0 disables |
| `read-timeout` | disabled | Per-read timeout in nanoseconds; 0 disables |
| `write-timeout` | 10s | Per-write timeout in nanoseconds; 0 disables |
| `graceful-shutdown-timeout` | 0 | Nanoseconds to wait for an active publisher to close during listener shutdown; 0 closes immediately |
| `keep-listening` | `false` | Wait for another publisher after disconnect instead of returning EOS |

## Behaviour

By default, the listener accepts one publisher and emits EOS after a clean
unpublish or publisher connection close. A close without unpublishing is logged
as a warning but still becomes EOS, so downstream queues and muxers can drain
rather than hang.

With `keep-listening=true`, the listener keeps its TCP socket open after the
publisher disconnects and waits for another publisher. It emits no buffers or
GAP events during the outage, and posts an element bus message named
`connection-removed`. Buffers resume when the next publisher connects; the first
resumed buffer is marked `DISCONT`. Each publisher session emits one FLV header,
including after a reconnect. A publisher with a mismatched
application or stream key is rejected without stopping the listener.

When `graceful-shutdown-timeout` is greater than zero, listener shutdown sends
the RTMP `NetConnection.Connect.Closed` status to an established publisher and
waits up to the configured duration for it to close. A value of zero preserves
the immediate socket close behavior.

Publisher lifecycle is also available in-band on the source pad as serialized,
non-sticky `CUSTOM_DOWNSTREAM` events. For each accepted publisher, the source
pushes `scufflertmp-publish-start` immediately before its first FLV buffer and
`scufflertmp-publish-end` immediately after its final FLV buffer. Both events
contain a `connection-id` field (`uint64`). The end event also contains a
`reason` field: `unpublished`, `disconnect`, or `error`. On reconnect, the
normal new-stream `STREAM_START`, `CAPS`, and `SEGMENT` events precede the next
`scufflertmp-publish-start`; the lifecycle events are not sticky and are never
replayed to a later downstream connection.

## Enhanced RTMP and multitrack

The underlying session implements [Enhanced RTMP] v2, including the multitrack
capability, and the element relays the FLV stream verbatim — so a publisher
sending several video tracks over one connection reaches `flvdemux` as several
pads. Ask for them by name:

```bash
gst-launch-1.0 -e scufflertmplistensrc port=1935 ! queue ! flvdemux name=demux demux.video ! queue ! h264parse ! fakesink demux.video_1 ! queue ! h264parse ! fakesink demux.video_2 ! queue ! h264parse ! fakesink demux.audio ! queue ! aacparse ! fakesink
```

Publish three renditions and one audio track into it with:

```bash
ffmpeg -re -i input.mp4 -filter_complex '[0:v:0]split=3[a][b][c];[b]scale=1280:720[b2];[c]scale=640:360[c2]' -map '[a]' -map '[b2]' -map '[c2]' -map 0:a:0 -c:v libx264 -preset ultrafast -bf 0 -b:v:0 4500k -b:v:1 2500k -b:v:2 800k -c:a copy -f flv rtmp://127.0.0.1:1935/live/test
```

This is covered by the integration suite, not just the specification: see
`test_multitrack` in `tests/integration.sh`.

Because the element passes FLV through untouched, support for codecs beyond
H.264/AAC — HEVC, AV1, Opus and the rest of the Enhanced RTMP set — is a
question of what your `flvdemux` and decoders handle, not of this element.

[Enhanced RTMP]: https://github.com/veovera/enhanced-rtmp

## Buffering

The source uses a one-buffer internal channel purely to hand data from the RTMP
worker to GStreamer's source task. It is not an ingest buffer. Add a `queue`
immediately after the source when you want one — this allows up to 16 MiB while
disabling the other limits:

```text
scufflertmplistensrc ! queue max-size-buffers=0 max-size-bytes=16777216 max-size-time=0 ! flvdemux
```

When that queue fills, backpressure propagates through the one-buffer handoff to
the RTMP socket. Queues *after* `flvdemux` serve a different purpose: they
decouple the audio and video branches from each other.

## Debugging

```bash
GST_DEBUG=scufflertmplistensrc:6 gst-launch-1.0 ...
```

Logs listener start/stop, connection, publish, unpublish, rejection, and session
completion/failure. Stream keys are deliberately never written to the log.

## Tests

```bash
cargo build -p gst-scuffle-rtmp && ./scuffle-rtmp/tests/integration.sh
```

Requires `gst-launch-1.0`, `ffmpeg`, and `ffprobe`. The suite synthesises its own
fixture; set `FIXTURE=/path/to.mp4` to use real footage instead, and `PORT_BASE`
to move off the default five-port range.

It covers publisher accept timeout, stream-key rejection, clean A/V remux and
EOS, abrupt-disconnect EOS draining, graceful listener shutdown,
keep-listening reconnects, serialized publisher lifecycle event ordering, and
three-video-track plus audio multiplexing.

## Design notes

### Vendored dependency

`vendor/scuffle-rtmp` is [scuffle-rtmp] 0.2.3 as published to crates.io, with
local changes. Upstream hardcodes its network timeouts and does not expose the
chunk timestamp and acknowledgement-window behaviour this element needs;
`vendor/scuffle-rtmp/LOCAL_CHANGES.md` explains what was changed and why.

Vendoring usually means losing track of how far you have drifted from upstream,
so the divergence is recorded as a patch rather than left implicit:

```bash
./vendor/verify.sh
```

That downloads the pristine crate, applies `vendor/local-changes.patch`, and
diffs the result against the vendored tree. CI runs it, so the patch cannot
quietly stop describing the code. If you edit the vendored source, regenerate
the patch:

```bash
diff -ruN -x target -x .cargo-checksum.json <pristine-crate-dir> vendor/scuffle-rtmp \
  > vendor/local-changes.patch
```

## License

MIT or Apache-2.0, at your option. The vendored `scuffle-rtmp` carries upstream's
MIT/Apache-2.0 licensing.

[scuffle-rtmp]: https://github.com/scufflecloud/scuffle
