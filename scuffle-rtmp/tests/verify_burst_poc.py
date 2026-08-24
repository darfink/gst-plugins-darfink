#!/usr/bin/env python3
"""Verify that one five-second burst survives the selector/synchronizer path."""

from __future__ import annotations

import argparse
import hashlib
import subprocess
from collections import Counter
from pathlib import Path


def run(command: list[str]) -> bytes:
    return subprocess.check_output(command, stderr=subprocess.PIPE)


def video_pts(path: Path) -> list[float]:
    output = run(
        [
            "ffprobe",
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "frame=best_effort_timestamp_time",
            "-of",
            "csv=p=0",
            str(path),
        ]
    ).decode()
    return [
        float(line.rstrip(",").split(",")[0])
        for line in output.splitlines()
        if line.strip() and line.rstrip(",").split(",")[0] not in {"N/A", ""}
    ]


def video_hashes(path: Path, width: int, height: int) -> list[str]:
    output = run(
        [
            "ffmpeg",
            "-hide_banner",
            "-loglevel",
            "error",
            "-i",
            str(path),
            "-map",
            "0:v:0",
            "-fps_mode",
            "passthrough",
            "-pix_fmt",
            "yuv420p",
            "-f",
            "rawvideo",
            "-",
        ]
    )
    frame_size = width * height * 3 // 2
    luma_size = width * height
    if len(output) % frame_size:
        raise RuntimeError("decoded video ended with a partial frame")
    return [
        hashlib.blake2b(
            output[offset : offset + luma_size], digest_size=8
        ).hexdigest()
        for offset in range(0, len(output), frame_size)
    ]


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--input", type=Path, required=True)
    parser.add_argument("--publish-elapsed", type=float, required=True)
    args = parser.parse_args()

    if args.publish_elapsed >= 1.0:
        raise RuntimeError(
            f"the five-second fixture was not published in under one second: "
            f"{args.publish_elapsed:.3f}s"
        )

    pts = video_pts(args.input)
    hashes = video_hashes(args.input, width=320, height=180)
    if len(pts) != len(hashes):
        raise RuntimeError(
            f"ffprobe returned {len(pts)} video timestamps for {len(hashes)} frames"
        )
    if len(hashes) < 150:
        raise RuntimeError(f"expected at least 150 decoded frames, found {len(hashes)}")

    # The fallback image is a single repeated frame. Find its first sustained
    # run and treat everything before it as the burst. This also catches a
    # dropped/repeated live frame at the burst-to-fallback boundary.
    fallback_start = None
    for index in range(1, len(hashes) - 59):
        if len(set(hashes[index : index + 60])) == 1:
            fallback_start = index
            break
    if fallback_start is None:
        raise RuntimeError("could not find the sustained static fallback run")
    if not 145 <= fallback_start <= 165:
        raise RuntimeError(
            f"expected the five-second burst to end near frame 150, "
            f"found static fallback at frame {fallback_start}"
        )
    moving_unique = len(set(hashes[:fallback_start]))
    if moving_unique < 120:
        raise RuntimeError(
            f"burst video is not changing enough: {moving_unique} unique frames"
        )

    live_pts = pts[:fallback_start]
    deltas = [round(after - before, 6) for before, after in zip(live_pts, live_pts[1:])]
    if any(abs(delta - 1 / 30) > 0.001 for delta in deltas):
        raise RuntimeError(f"burst video cadence is not 30 fps: {Counter(deltas)}")
    span = live_pts[-1] - live_pts[0]
    if not 4.8 <= span <= 5.1:
        raise RuntimeError(f"burst timestamps span {span:.3f}s instead of about 5s")

    print(
        f"verified: publish={args.publish_elapsed:.3f}s, burst frames={fallback_start}, "
        f"moving unique={moving_unique}, timestamp span={span:.3f}s, "
        f"output frames={len(hashes)}",
        flush=True,
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
