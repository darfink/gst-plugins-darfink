# Build and register the text roll-up element without installing Rust or
# GStreamer development packages locally.
#
#   docker build -t gst-textrollup .
#   docker run --rm gst-textrollup
#
# The default command verifies that the plugin loads. The speech-to-text
# companion and a model are deliberately not bundled in this image.
FROM ubuntu:24.04 AS builder

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

# Build dependencies first so editing the element does not rebuild the
# dependency graph. This project intentionally does not commit Cargo.lock.
COPY Cargo.toml ./
RUN mkdir -p src && echo '' > src/lib.rs && cargo build --release || true

COPY src ./src
RUN touch src/lib.rs && cargo build --release

FROM ubuntu:24.04 AS runtime

ARG DEBIAN_FRONTEND=noninteractive

RUN apt-get update && \
    apt-get install -y --no-install-recommends \
      gstreamer1.0-plugins-base \
      gstreamer1.0-plugins-good \
      gstreamer1.0-tools \
    && rm -rf /var/lib/apt/lists/*

# Ubuntu scans a multiarch plugin directory, so use an explicit private path.
COPY --from=builder /src/target/release/libgsttextrollup.so /usr/local/lib/gstreamer-1.0/
ENV GST_PLUGIN_PATH=/usr/local/lib/gstreamer-1.0

# Fail the image build rather than ship a plugin that does not load.
RUN gst-inspect-1.0 textrollup > /dev/null

CMD ["gst-inspect-1.0", "textrollup"]
