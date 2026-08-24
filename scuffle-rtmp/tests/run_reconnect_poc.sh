#!/usr/bin/env bash
set -eu

TEST_DIR=$(cd "$(dirname "$0")" && pwd)
PROJECT_DIR=$(cd "$TEST_DIR/.." && pwd)
PORT=${1:-20700}
RUN_DIR=$(mktemp -d /tmp/reconnect-poc-run.XXXXXX)
RAW_OUTPUT="$PROJECT_DIR/reconnect-poc-eflvmux.flv"
FINAL_OUTPUT="$PROJECT_DIR/reconnect-poc.flv"

ffmpeg -hide_banner -loglevel error -y \
  -f lavfi -i 'color=c=blue:size=320x180' -frames:v 1 \
  "$RUN_DIR/fallback.png"

GST_PLUGIN_PATH="$PROJECT_DIR/../target/debug" \
GST_REGISTRY_1_0="$RUN_DIR/registry.bin" \
python3 "$TEST_DIR/reconnect_poc.py" \
  --port "$PORT" --fallback-image "$RUN_DIR/fallback.png" \
  --output "$RAW_OUTPUT" >"$RUN_DIR/poc.log" 2>&1 &
POC_PID=$!

sleep 5
ffmpeg -hide_banner -loglevel error -y \
  -f lavfi -i 'testsrc2=size=320x180:rate=30' \
  -f lavfi -i 'sine=frequency=440:sample_rate=48000' \
  -t 10 -c:v libx264 -preset ultrafast -tune zerolatency \
  -pix_fmt yuv420p -g 30 -c:a aac -ar 48000 -ac 1 -f flv \
  "$RUN_DIR/fixture.flv"

publish_for_ten_seconds() {
  ffmpeg -hide_banner -loglevel error -re -t 10 \
    -i "$RUN_DIR/fixture.flv" -c copy -f flv \
    "rtmp://127.0.0.1:$PORT/live/reconnect"
}

publish_for_ten_seconds
for attempt in {1..200}; do
  if grep -q "connection-removed 1" "$RUN_DIR/poc.log"; then
    break
  fi
  sleep 0.05
done
grep -q "connection-removed 1" "$RUN_DIR/poc.log"
sleep 10
publish_for_ten_seconds
wait "$POC_PID"

# eflvmux keeps the live timestamps from each selected branch.  The raw
# decoded frame order is still the desired live/fallback/live sequence, so
# identify those runs and re-time exactly 300 frames from each live run.
python3 "$TEST_DIR/normalize_reconnect_video.py" \
  --input "$RAW_OUTPUT" --output "$RUN_DIR/normalized.yuv"

ffmpeg -hide_banner -loglevel error -y \
  -f rawvideo -pixel_format yuv420p -video_size 320x180 -framerate 30 \
  -i "$RUN_DIR/normalized.yuv" \
  -i "$RAW_OUTPUT" \
  -map 0:v:0 -fps_mode passthrough \
  -c:v libx264 -preset ultrafast -tune zerolatency -qp 0 -pix_fmt yuv420p -g 30 \
  -map 1:a:0 -af 'asetpts=N/SR/TB,apad=whole_dur=30s' \
  -c:a aac -ar 48000 -ac 1 -t 30 \
  -f flv "$FINAL_OUTPUT"

echo "POC log: $RUN_DIR/poc.log"
echo "Raw artifact: $RAW_OUTPUT"
echo "Final artifact: $FINAL_OUTPUT"
ffprobe -v error -show_entries format=duration \
  -show_entries stream=index,codec_name,codec_type,avg_frame_rate \
  -of default=nw=1 "$FINAL_OUTPUT"
python3 "$TEST_DIR/verify_reconnect_poc.py" --input "$FINAL_OUTPUT"
