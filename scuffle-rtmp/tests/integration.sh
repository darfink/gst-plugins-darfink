#!/usr/bin/env bash

set -euo pipefail

repo_root=${REPO_ROOT:-$(git rev-parse --show-toplevel)}
test_dir=$(cd "$(dirname "$0")" && pwd)
plugin_path=${GST_PLUGIN_PATH:-"$repo_root/target/debug"}
port_base=${PORT_BASE:-20540}
tmp_dir=$(mktemp -d /tmp/gst-scuffle-rtmp-integration.XXXXXX)
# Synthesised so the suite is self-contained. Point FIXTURE at real footage to
# exercise the listener against a more representative bitstream.
fixture=${FIXTURE:-"$tmp_dir/fixture.mp4"}
registry="$tmp_dir/registry.bin"
gst_pid=
publisher_pid=
probe_pid=

cleanup_processes() {
  if [[ -n $publisher_pid ]] && kill -0 "$publisher_pid" 2>/dev/null; then
    kill "$publisher_pid" 2>/dev/null || true
    wait "$publisher_pid" 2>/dev/null || true
  fi
  if [[ -n $gst_pid ]] && kill -0 "$gst_pid" 2>/dev/null; then
    kill -INT "$gst_pid" 2>/dev/null || true
    wait "$gst_pid" 2>/dev/null || true
  fi
  if [[ -n $probe_pid ]] && kill -0 "$probe_pid" 2>/dev/null; then
    kill -INT "$probe_pid" 2>/dev/null || true
    wait "$probe_pid" 2>/dev/null || true
  fi
  publisher_pid=
  gst_pid=
  probe_pid=
}

cleanup() {
  cleanup_processes
  rm -rf -- "$tmp_dir"
}
trap cleanup EXIT INT TERM

fail() {
  echo "FAIL: $*" >&2
  exit 1
}

check_port() {
  local port=$1
  if lsof -nP -iTCP:"$port" -sTCP:LISTEN >/dev/null 2>&1; then
    fail "TCP port $port is already in use"
  fi
}

wait_for_listener() {
  local log=$1
  local attempt
  for attempt in {1..100}; do
    if grep -q "Listening for one RTMP publisher" "$log" 2>/dev/null; then
      return 0
    fi
    if ! kill -0 "$gst_pid" 2>/dev/null; then
      sed -n '1,160p' "$log" >&2
      fail "GStreamer pipeline exited before listening"
    fi
    sleep 0.05
  done
  fail "GStreamer pipeline did not begin listening"
}

wait_for_log() {
  local log=$1
  local pattern=$2
  local attempt
  for attempt in {1..100}; do
    if grep -q "$pattern" "$log" 2>/dev/null; then
      return 0
    fi
    if ! kill -0 "$gst_pid" 2>/dev/null; then
      sed -n '1,160p' "$log" >&2
      fail "GStreamer pipeline exited before logging: $pattern"
    fi
    sleep 0.05
  done
  sed -n '1,160p' "$log" >&2
  fail "GStreamer pipeline did not log: $pattern"
}

wait_for_probe_log() {
  local log=$1
  local pattern=$2
  local attempt
  for attempt in {1..100}; do
    if grep -q "$pattern" "$log" 2>/dev/null; then
      return 0
    fi
    if ! kill -0 "$probe_pid" 2>/dev/null; then
      sed -n '1,160p' "$log" >&2
      fail "Lifecycle probe exited before logging: $pattern"
    fi
    sleep 0.05
  done
  sed -n '1,160p' "$log" >&2
  fail "Lifecycle probe did not log: $pattern"
}

start_pipeline() {
  local log=$1
  shift
  GST_PLUGIN_PATH="$plugin_path" \
    GST_REGISTRY="$registry" \
    GST_DEBUG=scufflertmplistensrc:6 \
    gst-launch-1.0 "$@" >"$log" 2>&1 &
  gst_pid=$!
  wait_for_listener "$log"
}

wait_for_success() {
  local log=$1
  if ! wait "$gst_pid"; then
    sed -n '1,200p' "$log" >&2
    fail "GStreamer pipeline failed"
  fi
  gst_pid=
}

wait_for_failure() {
  local log=$1
  local pattern=$2
  if wait "$gst_pid"; then
    fail "GStreamer pipeline unexpectedly succeeded"
  fi
  gst_pid=
  grep -q "$pattern" "$log" || {
    sed -n '1,200p' "$log" >&2
    fail "Pipeline did not report expected error: $pattern"
  }
}

test_accept_timeout() {
  local port=$((port_base + 0))
  local log="$tmp_dir/accept-timeout.log"
  check_port "$port"

  if GST_PLUGIN_PATH="$plugin_path" \
    GST_REGISTRY="$registry" \
    gst-launch-1.0 \
      scufflertmplistensrc address=127.0.0.1 port="$port" accept-timeout=200000000 \
      ! fakesink >"$log" 2>&1; then
    fail "Accept-timeout pipeline unexpectedly succeeded"
  fi
  grep -q "Timed out waiting" "$log" || fail "Accept timeout was not reported"
  echo "PASS accept timeout"
}

test_rejected_stream_key() {
  local port=$((port_base + 1))
  local log="$tmp_dir/rejected-key.log"
  check_port "$port"
  start_pipeline "$log" \
    scufflertmplistensrc address=127.0.0.1 port="$port" application=live stream-key=allowed \
    ! fakesink

  ffmpeg -hide_banner -loglevel error -i "$fixture" -t 1 \
    -map 0:v:0 -an -c:v copy -f flv \
    "rtmp://127.0.0.1:$port/live/denied" >"$tmp_dir/rejected-key-ffmpeg.log" 2>&1 || true
  wait_for_failure "$log" "did not match the configured application and stream key"
  echo "PASS rejected stream key"
}

test_rejected_stream_key_keep_listening() {
  local port=$((port_base + 6))
  local log="$tmp_dir/rejected-key-keep-listening.log"
  check_port "$port"
  start_pipeline "$log" -m \
    scufflertmplistensrc address=127.0.0.1 port="$port" application=live stream-key=allowed keep-listening=true \
    ! fakesink

  ffmpeg -hide_banner -loglevel error -i "$fixture" -t 1 \
    -map 0:v:0 -an -c:v copy -f flv \
    "rtmp://127.0.0.1:$port/wrong/stream" >"$tmp_dir/rejected-key-keep-ffmpeg.log" 2>&1 || true
  wait_for_log "$log" "Rejected RTMP publish"
  kill -0 "$gst_pid" 2>/dev/null || fail "keep-listening pipeline stopped after a rejected publisher"
  grep -q "Could not read from resource" "$log" &&
    fail "Rejected publisher caused a keep-listening source error"

  ffmpeg -hide_banner -loglevel error -i "$fixture" -t 1 \
    -map 0:v:0 -an -c:v copy -f flv \
    "rtmp://127.0.0.1:$port/live/allowed" >"$tmp_dir/rejected-key-keep-valid-ffmpeg.log" 2>&1
  wait_for_log "$log" "Accepted RTMP publish"
  kill -INT "$gst_pid" 2>/dev/null || true
  wait "$gst_pid" 2>/dev/null || true
  gst_pid=
  echo "PASS rejected stream key with keep-listening"
}

test_clean_av_and_eos() {
  local port=$((port_base + 2))
  local log="$tmp_dir/clean-av.log"
  local output="$tmp_dir/clean-av.flv"
  check_port "$port"
  start_pipeline "$log" -e \
    scufflertmplistensrc address=127.0.0.1 port="$port" application=live stream-key=av \
    ! queue max-size-buffers=0 max-size-bytes=1048576 max-size-time=0 \
    ! flvdemux name=demux \
    flvmux name=mux ! filesink location="$output" \
    demux.video ! queue ! h264parse ! mux.video \
    demux.audio ! queue ! aacparse ! mux.audio

  ffmpeg -hide_banner -loglevel error -i "$fixture" -t 2 \
    -map 0:v:0 -map 0:a:0 -c copy -f flv \
    "rtmp://127.0.0.1:$port/live/av"
  wait_for_success "$log"
  grep -q "Got EOS" "$log" || fail "Clean publish did not produce EOS"
  ffprobe -v error -select_streams v:0 -show_entries stream=codec_name \
    -of default=nw=1:nk=1 "$output" | grep -qx h264 || fail "Remuxed video is not H.264"
  ffprobe -v error -select_streams a:0 -show_entries stream=codec_name \
    -of default=nw=1:nk=1 "$output" | grep -qx aac || fail "Remuxed audio is not AAC"
  echo "PASS clean A/V and EOS"
}

test_abrupt_disconnect_drains_eos() {
  local port=$((port_base + 3))
  local log="$tmp_dir/abrupt-disconnect.log"
  check_port "$port"
  start_pipeline "$log" \
    scufflertmplistensrc address=127.0.0.1 port="$port" application=live stream-key=abrupt \
    ! fakesink

  ffmpeg -hide_banner -loglevel error -re -stream_loop -1 -i "$fixture" \
    -map 0:v:0 -map 0:a:0 -c copy -f flv \
    "rtmp://127.0.0.1:$port/live/abrupt" >"$tmp_dir/abrupt-ffmpeg.log" 2>&1 &
  publisher_pid=$!
  sleep 1
  kill -KILL "$publisher_pid"
  wait "$publisher_pid" 2>/dev/null || true
  publisher_pid=
  wait_for_success "$log"
  grep -q "disconnected without unpublishing" "$log" ||
    fail "Abrupt disconnect was not reported"
  grep -q "Got EOS" "$log" || fail "Abrupt disconnect did not produce EOS"
  echo "PASS abrupt disconnect drains as EOS"
}

test_graceful_shutdown_notifies_publisher() {
  local port=$((port_base + 8))
  local log="$tmp_dir/graceful-shutdown.log"
  check_port "$port"
  start_pipeline "$log" -e \
    scufflertmplistensrc address=127.0.0.1 port="$port" application=live stream-key=graceful \
    graceful-shutdown-timeout=500000000 ! fakesink num-buffers=250 sync=false

  ffmpeg -hide_banner -loglevel error -re -stream_loop -1 -i "$fixture" \
    -map 0:v:0 -map 0:a:0 -c copy -f flv \
    "rtmp://127.0.0.1:$port/live/graceful" >"$tmp_dir/graceful-ffmpeg.log" 2>&1 &
  publisher_pid=$!
  wait_for_log "$log" "Accepted RTMP publish"
  wait_for_success "$log"
  wait "$publisher_pid" 2>/dev/null || true
  publisher_pid=
  grep -q "Got EOS" "$log" || fail "Graceful shutdown pipeline did not produce EOS"
  echo "PASS graceful listener shutdown"
}

test_keep_listening_reconnects() {
  local port=$((port_base + 5))
  local log="$tmp_dir/keep-listening.log"
  check_port "$port"
  start_pipeline "$log" -m \
    scufflertmplistensrc address=127.0.0.1 port="$port" application=live stream-key=reconnect keep-listening=true \
    ! fakesink

  ffmpeg -hide_banner -loglevel error -re -stream_loop -1 -i "$fixture" \
    -map 0:v:0 -map 0:a:0 -c copy -f flv \
    "rtmp://127.0.0.1:$port/live/reconnect" >"$tmp_dir/reconnect-first-ffmpeg.log" 2>&1 &
  publisher_pid=$!
  # Match the publish-accept line, not the TCP-accept line: killing ffmpeg
  # before it has published means there is no publish lifecycle to end and no
  # connection-removed event to wait for.
  wait_for_log "$log" "Accepted RTMP publish for app"
  kill -KILL "$publisher_pid" 2>/dev/null || true
  wait "$publisher_pid" 2>/dev/null || true
  publisher_pid=

  wait_for_log "$log" "connection-removed"
  kill -0 "$gst_pid" 2>/dev/null || fail "keep-listening pipeline stopped after disconnect"
  grep -q "Got EOS" "$log" && fail "keep-listening pipeline emitted EOS after disconnect"

  ffmpeg -hide_banner -loglevel error -re -i "$fixture" -t 2 \
    -map 0:v:0 -map 0:a:0 -c copy -f flv \
    "rtmp://127.0.0.1:$port/live/reconnect" >"$tmp_dir/reconnect-second-ffmpeg.log" 2>&1

  for attempt in {1..100}; do
    if [[ $(grep -c "Accepted RTMP publisher connection" "$log" 2>/dev/null || true) -ge 2 ]]; then
      break
    fi
    sleep 0.05
  done
  [[ $(grep -c "Accepted RTMP publisher connection" "$log" 2>/dev/null || true) -ge 2 ]] ||
    fail "keep-listening pipeline did not accept the second publisher"
  grep -q "Got EOS" "$log" && fail "keep-listening pipeline emitted EOS after reconnect"

  kill -INT "$gst_pid" 2>/dev/null || true
  wait "$gst_pid" 2>/dev/null || true
  gst_pid=
  echo "PASS keep-listening reconnect"
}

test_publish_lifecycle_events() {
  local port=$((port_base + 7))
  local log="$tmp_dir/publish-lifecycle.log"
  check_port "$port"

  GST_PLUGIN_PATH="$plugin_path" \
    GST_REGISTRY="$registry" \
    python3 "$test_dir/publish_lifecycle_probe.py" \
      --port "$port" --publishers 2 >"$log" 2>&1 &
  probe_pid=$!
  wait_for_probe_log "$log" "LISTENING"

  ffmpeg -hide_banner -loglevel error -re -stream_loop -1 -i "$fixture" \
    -map 0:v:0 -map 0:a:0 -c copy -f flv \
    "rtmp://127.0.0.1:$port/live/lifecycle" >"$tmp_dir/lifecycle-first-ffmpeg.log" 2>&1 &
  publisher_pid=$!
  wait_for_probe_log "$log" '"kind":"scufflertmp-publish-start","connection-id":1'
  kill -KILL "$publisher_pid" 2>/dev/null || true
  wait "$publisher_pid" 2>/dev/null || true
  publisher_pid=
  wait_for_probe_log "$log" '"kind":"scufflertmp-publish-end","connection-id":1'

  ffmpeg -hide_banner -loglevel error -re -i "$fixture" -t 2 \
    -map 0:v:0 -map 0:a:0 -c copy -f flv \
    "rtmp://127.0.0.1:$port/live/lifecycle" >"$tmp_dir/lifecycle-second-ffmpeg.log" 2>&1

  if ! wait "$probe_pid"; then
    sed -n '1,240p' "$log" >&2
    fail "Lifecycle probe failed"
  fi
  probe_pid=
  echo "PASS publish lifecycle events"
}

test_multitrack() {
  local port=$((port_base + 4))
  local log="$tmp_dir/multitrack.log"
  check_port "$port"
  start_pipeline "$log" -e \
    scufflertmplistensrc address=127.0.0.1 port="$port" application=live stream-key=multitrack \
    ! queue max-size-buffers=0 max-size-bytes=16777216 max-size-time=0 \
    ! flvdemux name=demux \
    demux.video ! queue ! h264parse ! fakesink sync=false \
    demux.video_1 ! queue ! h264parse ! fakesink sync=false \
    demux.video_2 ! queue ! h264parse ! fakesink sync=false \
    demux.audio ! queue ! aacparse ! fakesink sync=false

  ffmpeg -hide_banner -loglevel warning -i "$fixture" -t 2 \
    -filter_complex \
      '[0:v:0]split=3[v1080][v720in][v360in];[v720in]scale=1280:720[v720];[v360in]scale=640:360[v360]' \
    -map '[v1080]' -map '[v720]' -map '[v360]' -map 0:a:0 \
    -c:v libx264 -preset ultrafast -tune zerolatency \
    -g 48 -keyint_min 48 -sc_threshold 0 -bf 0 \
    -b:v:0 4500k -b:v:1 2500k -b:v:2 800k -c:a copy -f flv \
    "rtmp://127.0.0.1:$port/live/multitrack"
  wait_for_success "$log"
  grep -q "Got EOS" "$log" || fail "Multitrack publish did not produce EOS"
  echo "PASS multitrack"
}

if [[ ! -f $fixture ]]; then
  echo "Generating fixture at $fixture"
  ffmpeg -hide_banner -loglevel error -y \
    -f lavfi -i "testsrc2=size=640x360:rate=30" \
    -f lavfi -i "sine=frequency=440:sample_rate=48000" \
    -t 12 -c:v libx264 -preset ultrafast -pix_fmt yuv420p -g 60 -bf 0 \
    -c:a aac -b:a 128k "$fixture" ||
    fail "Could not generate fixture; supply one with FIXTURE=/path/to.mp4"
fi
[[ -f $fixture ]] || fail "Fixture not found: $fixture"
[[ -d $plugin_path ]] || fail "Plugin build directory not found: $plugin_path"

test_accept_timeout
test_rejected_stream_key
test_rejected_stream_key_keep_listening
test_clean_av_and_eos
test_abrupt_disconnect_drains_eos
test_graceful_shutdown_notifies_publisher
test_keep_listening_reconnects
test_publish_lifecycle_events
test_multitrack

echo "All scufflertmplistensrc integration tests passed"
