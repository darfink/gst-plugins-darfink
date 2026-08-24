#!/usr/bin/env bash
set -eu

TEST_DIR=$(cd "$(dirname "$0")" && pwd)
PROJECT_DIR=$(cd "$TEST_DIR/.." && pwd)
INPUT=${1:-/Users/atomen/Projects/rushls/tests/fixtures/satya-10min-gop2s-bf0.mp4}
PORT=${2:-20750}
SYNC_STREAMS=${3:-true}
POST_SYNC_SINGLE_SEGMENT=${4:-false}
ARTIFACT_STEM=${5:-reconnect-satya-50s}
GENERATION_RATE_NO_CLOSING_DUPLICATES=${6:-false}
SHARED_VIDEO_RATE=${7:-false}
SHARED_AUDIO_RATE=${8:-false}
CACHE_BUFFERS=${9:-false}
PREROLL_BEFORE_SWITCH=${10:-false}
RAW_ONLY=${11:-false}
NORMALIZE_SYNC_TIMESTAMPS=${12:-false}
STOP_AFTER_FINAL_GENERATION=${13:-false}
DISCONNECT_WAIT_SECONDS=${14:-10}
RUN_DIR=$(mktemp -d /tmp/reconnect-satya-poc.XXXXXX)
RAW_OUTPUT="$PROJECT_DIR/${ARTIFACT_STEM}-eflvmux.flv"
FINAL_OUTPUT="$PROJECT_DIR/${ARTIFACT_STEM}.flv"

if [ ! -f "$INPUT" ]; then
  echo "input does not exist: $INPUT" >&2
  exit 1
fi

ffmpeg -hide_banner -loglevel error -y \
  -f lavfi -i 'color=c=blue:size=320x180' -frames:v 1 \
  "$RUN_DIR/fallback.png"

# Use the supplied media as the content source, while making each publisher
# segment exactly 10 seconds and 300 frames before it enters the POC.
ffmpeg -hide_banner -loglevel error -y \
  -i "$INPUT" -map 0:v:0 -map 0:a:0 -t 10 \
  -vf 'scale=320:180,fps=30' \
  -c:v libx264 -preset ultrafast -tune zerolatency -pix_fmt yuv420p -g 30 -bf 0 \
  -c:a aac -ar 48000 -ac 1 -shortest -f flv \
  "$RUN_DIR/fixture.flv"

GST_PLUGIN_PATH="$PROJECT_DIR/../target/debug" \
GST_REGISTRY_1_0="$RUN_DIR/registry.bin" \
python3 "$TEST_DIR/reconnect_poc.py" \
  --port "$PORT" --fallback-image "$RUN_DIR/fallback.png" \
  --output "$RAW_OUTPUT" \
  --expected-generations 3 --expected-disconnects 3 \
  --selector-sync-streams "$SYNC_STREAMS" \
  --selector-sync-mode clock \
  --selector-cache-buffers "$CACHE_BUFFERS" \
  --output-sync false \
  --mux-enforce-increasing-timestamps false \
  --generation-duration-seconds 10 \
  --post-sync-single-segment "$POST_SYNC_SINGLE_SEGMENT" \
  --generation-rate-no-closing-duplicates "$GENERATION_RATE_NO_CLOSING_DUPLICATES" \
  --shared-video-rate "$SHARED_VIDEO_RATE" \
  --shared-audio-rate "$SHARED_AUDIO_RATE" \
  --preroll-before-switch "$PREROLL_BEFORE_SWITCH" \
  --normalize-sync-timestamps "$NORMALIZE_SYNC_TIMESTAMPS" \
  --stop-after-final-generation "$STOP_AFTER_FINAL_GENERATION" >"$RUN_DIR/poc.log" 2>&1 &
POC_PID=$!

sleep 5

publish_for_ten_seconds() {
  ffmpeg -hide_banner -loglevel error -re -t 10 \
    -i "$RUN_DIR/fixture.flv" -c copy -f flv \
    "rtmp://127.0.0.1:$PORT/live/reconnect"
}

wait_for_disconnect() {
  local number=$1
  for _attempt in {1..240}; do
    if grep -q "connection-removed $number" "$RUN_DIR/poc.log"; then
      return 0
    fi
    sleep 0.05
  done
  echo "timed out waiting for connection-removed $number" >&2
  tail -80 "$RUN_DIR/poc.log" >&2 || true
  return 1
}

publish_for_ten_seconds
wait_for_disconnect 1
sleep "$DISCONNECT_WAIT_SECONDS"

publish_for_ten_seconds
wait_for_disconnect 2
sleep "$DISCONNECT_WAIT_SECONDS"

publish_for_ten_seconds
wait_for_disconnect 3
wait "$POC_PID"

echo "POC log: $RUN_DIR/poc.log"
echo "Raw artifact: $RAW_OUTPUT"
if [ "$RAW_ONLY" = "true" ]; then
  ffprobe -v error \
    -show_entries format=duration \
    -show_entries stream=index,codec_name,codec_type,avg_frame_rate,sample_rate,channels \
    -of default=nw=1 "$RAW_OUTPUT"
  exit 0
fi

python3 "$TEST_DIR/normalize_reconnect_video.py" \
  --input "$RAW_OUTPUT" --output "$RUN_DIR/normalized.yuv" \
  --moving-segments 3 --frames-per-segment 300 \
  --max-boundary-motion-unique 10

ffmpeg -hide_banner -loglevel error -y \
  -f rawvideo -pixel_format yuv420p -video_size 320x180 -framerate 30 \
  -i "$RUN_DIR/normalized.yuv" \
  -i "$RAW_OUTPUT" \
  -map 0:v:0 -fps_mode passthrough \
  -c:v libx264 -preset ultrafast -tune zerolatency -qp 0 -pix_fmt yuv420p -g 30 \
  -map 1:a:0 -af 'asetpts=N/SR/TB,apad=whole_dur=50s' \
  -c:a aac -ar 48000 -ac 1 -t 50 \
  -f flv "$FINAL_OUTPUT"

echo "Final artifact: $FINAL_OUTPUT"
ffprobe -v error \
  -show_entries format=duration \
  -show_entries stream=index,codec_name,codec_type,avg_frame_rate,sample_rate,channels \
  -of default=nw=1 "$FINAL_OUTPUT"
python3 "$TEST_DIR/verify_reconnect_poc.py" \
  --input "$FINAL_OUTPUT" --active-segments 3 \
  --frames-per-segment 300 --segment-seconds 10
