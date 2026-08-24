# GStreamer plugins

A collection of GStreamer elements for live media ingest, captioning, and
speech recognition. Each element remains an independent Rust crate in this
workspace and can be built, tested, installed, and packaged separately. Each
plugin library registers one element.

## Elements

| Crate | GStreamer element | Purpose |
| --- | --- | --- |
| [`gst-flvsubmux`](flvsubmux/) | `flvsubmux` | Strictly aggregate timed or replacement captions into a muxed FLV stream. |
| [`gst-textrollup`](textrollup/) | `textrollup` | Render timestamped words into a stable roll-up caption window. |
| [`gst-transcribe-cpp`](transcribe-cpp/) | `transcribecpptranscriber` | Speech-to-text backed by `transcribe.cpp`. |
| [`gst-scuffle-rtmp`](scuffle-rtmp/) | `scufflertmplistensrc` | Accept an RTMP publisher and expose its FLV stream. |

The detailed documentation, properties, pipeline examples, and tests for each
element live in its directory:

- [FLV subtitle muxing](flvsubmux/README.md)
- [Text roll-up captions](textrollup/README.md)
- [Transcribe.cpp speech recognition](transcribe-cpp/README.md)
- [RTMP ingest](scuffle-rtmp/README.md)

## Build

The repository is a Cargo workspace with one shared lockfile. Build everything
with:

```bash
cargo build --workspace --release
```

Build an individual plugin with:

```bash
cargo build --release -p gst-flvsubmux
cargo build --release -p gst-textrollup
cargo build --release -p gst-transcribe-cpp --no-default-features
cargo build --release -p gst-scuffle-rtmp
```

The plugins require Rust and GStreamer development packages. The transcribe
plugin also needs CMake and a C++ toolchain; the RTMP plugin requires GStreamer
1.28 or newer. See the element README files for platform-specific details.

To try the built plugins with GStreamer:

```bash
export GST_PLUGIN_PATH="$PWD/target/release"
gst-inspect-1.0 flvsubmux textrollup transcribecpp scufflertmplistensrc
```

## Install

The plugins can be installed into the active GStreamer installation with
[`cargo-c`](https://github.com/lu-zero/cargo-c). Install it once with:

```bash
cargo install cargo-c --features=vendored-openssl
```

Then install the workspace plugins:

```bash
GSTREAMER_PREFIX="$(pkg-config --variable=prefix gstreamer-1.0)"
GSTREAMER_LIBDIR="$(pkg-config --variable=libdir gstreamer-1.0)"

cargo cinstall --release --library-type cdylib \
  -p gst-flvsubmux \
  -p gst-textrollup \
  -p gst-transcribe-cpp \
  -p gst-scuffle-rtmp \
  --prefix="$GSTREAMER_PREFIX" \
  --libdir="$GSTREAMER_LIBDIR"
```

The plugins are installed below `$GSTREAMER_LIBDIR/gstreamer-1.0`, which is the
directory reported by:

```bash
pkg-config --variable=pluginsdir gstreamer-1.0
```

## Testing

```bash
cargo test -p gst-flvsubmux -p gst-textrollup
cargo test -p gst-transcribe-cpp
cargo test -p gst-scuffle-rtmp
```

The RTMP integration suite additionally needs `gst-launch-1.0`, `ffmpeg`, and
`ffprobe`:

```bash
cargo build -p gst-scuffle-rtmp
./scuffle-rtmp/tests/integration.sh
```

## License

Each element keeps its own license and attribution. See the `LICENSE` file in
its directory.
