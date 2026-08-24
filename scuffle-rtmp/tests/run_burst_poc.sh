#!/usr/bin/env bash
set -eu

TEST_DIR=$(cd "$(dirname "$0")" && pwd)
PROJECT_DIR=$(cd "$TEST_DIR/.." && pwd)
PORT=${1:-20730}
SYNC_STREAMS=${2:-true}
SYNC_MODE=${3:-active-segment}
CACHE_BUFFERS=${4:-false}
RUN_DIR=$(mktemp -d /tmp/reconnect-burst-poc.XXXXXX)
RAW_OUTPUT="$RUN_DIR/burst-raw.flv"

ffmpeg -hide_banner -loglevel error -y \
  -f lavfi -i 'color=c=blue:size=320x180' -frames:v 1 \
  "$RUN_DIR/fallback.png"

ffmpeg -hide_banner -loglevel error -y \
  -f lavfi -i 'testsrc2=size=320x180:rate=30' \
  -f lavfi -i 'sine=frequency=440:sample_rate=48000' \
  -t 5 -c:v libx264 -preset ultrafast -tune zerolatency \
  -pix_fmt yuv420p -g 30 -c:a aac -ar 48000 -ac 1 -f flv \
  "$RUN_DIR/fixture.flv"

GST_PLUGIN_PATH="$PROJECT_DIR/../target/debug" \
GST_REGISTRY_1_0="$RUN_DIR/registry.bin" \
python3 "$TEST_DIR/reconnect_poc.py" \
  --port "$PORT" --fallback-image "$RUN_DIR/fallback.png" \
  --output "$RAW_OUTPUT" \
  --expected-generations 1 --expected-disconnects 1 \
  --selector-sync-streams "$SYNC_STREAMS" \
  --selector-sync-mode "$SYNC_MODE" \
  --selector-cache-buffers "$CACHE_BUFFERS" \
  --output-sync false \
  --mux-enforce-increasing-timestamps false >"$RUN_DIR/poc.log" 2>&1 &
POC_PID=$!

sleep 5
START_NS=$(python3 -c 'import time; print(time.monotonic_ns())')
ffmpeg -hide_banner -loglevel error \
  -i "$RUN_DIR/fixture.flv" -c copy -f flv \
  "rtmp://127.0.0.1:$PORT/live/reconnect"
END_NS=$(python3 -c 'import time; print(time.monotonic_ns())')
PUBLISH_ELAPSED=$(python3 - "$START_NS" "$END_NS" <<'PY'
import sys
print((int(sys.argv[2]) - int(sys.argv[1])) / 1_000_000_000)
PY
)

wait "$POC_PID"

echo "POC log: $RUN_DIR/poc.log"
echo "Raw artifact: $RAW_OUTPUT"
echo "selector sync-streams=$SYNC_STREAMS sync-mode=$SYNC_MODE cache-buffers=$CACHE_BUFFERS"
echo "publisher wall time: ${PUBLISH_ELAPSED}s"
ffprobe -v error \
  -show_entries format=duration \
  -show_entries stream=index,codec_name,codec_type,avg_frame_rate \
  -of default=nw=1 "$RAW_OUTPUT"
python3 "$TEST_DIR/verify_burst_poc.py" \
  --input "$RAW_OUTPUT" --publish-elapsed "$PUBLISH_ELAPSED"
