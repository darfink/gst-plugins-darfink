# Build the transcriber element and run it over a synthesised clip.
#
#   docker build -t gst-transcribe-cpp .
#   docker run --rm gst-transcribe-cpp
#
# To transcribe your own audio instead, mount it and pass it through:
#
#   docker run --rm -v "$PWD:/data" gst-transcribe-cpp \
#     /usr/local/bin/transcribe /models/model.gguf /data/speech.wav
#
# This is a CPU build. GPU backends are cargo features (cuda, vulkan); see the
# README.
FROM ubuntu:26.04 AS builder

ARG DEBIAN_FRONTEND=noninteractive
ARG RUST_VERSION=1.92.0

# cmake and a C++ toolchain are required: transcribe-cpp-sys vendors
# transcribe.cpp and builds it from source.
RUN apt-get update && \
    apt-get install -y --no-install-recommends \
      build-essential \
      ca-certificates \
      cmake \
      curl \
      libgstreamer1.0-dev \
      libgstreamer-plugins-base1.0-dev \
      ninja-build \
      pkg-config \
    && rm -rf /var/lib/apt/lists/*

RUN curl -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain "${RUST_VERSION}" --profile minimal
ENV PATH=/root/.cargo/bin:${PATH}

WORKDIR /src

# `metal` is the default feature and is macOS-only, hence --no-default-features.
#
# Build dependencies first so editing the element does not recompile ggml, which
# dominates this stage.
COPY Cargo.toml Cargo.lock ./
RUN mkdir -p src && echo '' > src/lib.rs && \
    cargo build --release --locked --no-default-features || true

COPY src ./src
COPY examples ./examples
RUN touch src/lib.rs && \
    cargo build --release --locked --no-default-features && \
    cargo build --release --locked --no-default-features --example transcribe

FROM ubuntu:26.04 AS runtime

ARG DEBIAN_FRONTEND=noninteractive
ARG MODEL=nemotron-speech-streaming-en-0.6b-Q8_0.gguf

RUN apt-get update && \
    apt-get install -y --no-install-recommends \
      ca-certificates \
      gstreamer1.0-plugins-base \
      gstreamer1.0-plugins-good \
      gstreamer1.0-tools \
      ffmpeg \
    && rm -rf /var/lib/apt/lists/*

# Debian and Ubuntu scan a multiarch plugin directory, so a plain /usr/lib drop
# would never be found.
COPY --from=builder /src/target/release/libgsttranscribecpp.so /usr/local/lib/gstreamer-1.0/
COPY --from=builder /src/target/release/examples/transcribe /usr/local/bin/transcribe
ENV GST_PLUGIN_PATH=/usr/local/lib/gstreamer-1.0

RUN gst-inspect-1.0 transcribecpptranscriber > /dev/null

ADD https://huggingface.co/handy-computer/nemotron-speech-streaming-en-0.6b-gguf/resolve/main/${MODEL} /models/${MODEL}
ENV TRANSCRIBE_MODEL=/models/${MODEL}

# A synthesised clip is silence, so it demonstrates that the element runs
# without pretending to demonstrate accuracy. Mount real audio to hear it work.
RUN ffmpeg -hide_banner -loglevel error -f lavfi -i anullsrc=r=16000:cl=mono \
      -t 5 -c:a pcm_s16le /sample.wav

CMD ["sh", "-c", "transcribe \"$TRANSCRIBE_MODEL\" /sample.wav"]
