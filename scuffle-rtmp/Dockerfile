# Build and run the RTMP listener element without installing GStreamer locally.
#
#   docker build -t gst-scuffle-rtmp -f scuffle-rtmp/Dockerfile .
#   docker run --rm -p 1935:1935 gst-scuffle-rtmp
#
# Then publish to rtmp://127.0.0.1:1935/live/test from the host.
#
# Ubuntu 26.04 is the base because it packages GStreamer 1.28, which this
# element's `v1_28` feature requires. Earlier LTS releases ship 1.24/1.26 and
# will not build it.
FROM ubuntu:26.04 AS builder

ARG DEBIAN_FRONTEND=noninteractive
ARG RUST_VERSION=1.92.0

RUN apt-get update && \
    apt-get install -y --no-install-recommends \
      build-essential \
      ca-certificates \
      curl \
      libgstreamer1.0-dev \
      libgstreamer-plugins-base1.0-dev \
      pkg-config \
    && rm -rf /var/lib/apt/lists/*

RUN curl -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain "${RUST_VERSION}" --profile minimal
ENV PATH=/root/.cargo/bin:${PATH}

WORKDIR /src

# Dependencies first, so editing the element does not rebuild the world. Only
# the member manifest is shipped; dependency versions are resolved at
# image-build time rather than pinned by the workspace lockfile, which covers
# all four plugins and cannot be used here.
COPY scuffle-rtmp/Cargo.toml scuffle-rtmp/build.rs ./
COPY scuffle-rtmp/vendor ./vendor
RUN mkdir -p src && echo '' > src/lib.rs && cargo build --release || true

COPY scuffle-rtmp/src ./src
RUN touch src/lib.rs && cargo build --release

FROM ubuntu:26.04 AS runtime

ARG DEBIAN_FRONTEND=noninteractive

RUN apt-get update && \
    apt-get install -y --no-install-recommends \
      gstreamer1.0-plugins-bad \
      gstreamer1.0-plugins-base \
      gstreamer1.0-plugins-good \
      gstreamer1.0-tools \
    && rm -rf /var/lib/apt/lists/*

# Not /usr/lib/gstreamer-1.0: Debian and Ubuntu put the system plugin directory
# under a multiarch path, so a plain /usr/lib drop is never scanned.
COPY --from=builder /src/target/release/libgstscufflertmp.so /usr/local/lib/gstreamer-1.0/
ENV GST_PLUGIN_PATH=/usr/local/lib/gstreamer-1.0

# Fail the build rather than ship an image whose plugin does not load.
RUN gst-inspect-1.0 scufflertmplistensrc > /dev/null

EXPOSE 1935

# Accept one publisher, demux it, and report what arrived. Override the command
# to build your own pipeline around the element.
CMD ["gst-launch-1.0", "-e", \
     "scufflertmplistensrc", "address=0.0.0.0", "port=1935", "!", "flvdemux", "name=demux", \
     "demux.video", "!", "queue", "!", "h264parse", "!", "fakesink", "sync=false", \
     "demux.audio", "!", "queue", "!", "aacparse", "!", "fakesink", "sync=false"]
