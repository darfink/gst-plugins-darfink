#!/usr/bin/env python3
"""Verify a normalized live/fallback sequence and its A/V alignment."""

from __future__ import annotations

import argparse
import hashlib
import math
import struct
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
        if line.strip()
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


def audio_rms(path: Path, sample_rate: int) -> list[float]:
    output = run(
        [
            "ffmpeg",
            "-hide_banner",
            "-loglevel",
            "error",
            "-i",
            str(path),
            "-map",
            "0:a:0",
            "-f",
            "s16le",
            "-ac",
            "1",
            "-ar",
            str(sample_rate),
            "-",
        ]
    )
    bytes_per_second = sample_rate * 2
    output = output[: len(output) - (len(output) % bytes_per_second)]
    result: list[float] = []
    for offset in range(0, len(output), bytes_per_second):
        samples = struct.unpack(
            f"<{sample_rate}h", output[offset : offset + bytes_per_second]
        )
        result.append(math.sqrt(sum(sample * sample for sample in samples) / sample_rate))
    return result


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--input", type=Path, required=True)
    parser.add_argument("--active-segments", type=int, default=2)
    parser.add_argument("--frames-per-segment", type=int, default=300)
    parser.add_argument("--segment-seconds", type=int, default=10)
    args = parser.parse_args()
    if args.active_segments < 2:
        raise RuntimeError("active-segments must be at least 2")

    pts = video_pts(args.input)
    expected_segments = 2 * args.active_segments - 1
    expected_frames = expected_segments * args.frames_per_segment
    if len(pts) != expected_frames:
        raise RuntimeError(
            f"expected {expected_frames} video frames, found {len(pts)}"
        )
    deltas = [round(after - before, 6) for before, after in zip(pts, pts[1:])]
    expected_delta = 1 / 30
    if any(abs(delta - expected_delta) > 0.001 for delta in deltas):
        raise RuntimeError(f"video cadence is not 30 fps: {Counter(deltas)}")

    hashes = video_hashes(args.input, width=320, height=180)
    if len(hashes) != expected_frames:
        raise RuntimeError(f"decoded video frame count changed: {len(hashes)}")
    moving_unique: list[int] = []
    fallback_unique: list[int] = []
    for segment in range(args.active_segments):
        active_start = segment * 2 * args.frames_per_segment
        active_end = active_start + args.frames_per_segment
        moving_unique.append(len(set(hashes[active_start:active_end])))
        if segment < args.active_segments - 1:
            fallback_start = active_end
            fallback_end = fallback_start + args.frames_per_segment
            fallback_unique.append(len(set(hashes[fallback_start:fallback_end])))
    if min(moving_unique) < 100:
        raise RuntimeError(f"moving segments are not changing: {moving_unique}")
    if max(fallback_unique) > 3:
        raise RuntimeError(f"fallback segments are not static: {fallback_unique}")

    rms = audio_rms(args.input, sample_rate=48000)
    expected_duration = expected_segments * args.segment_seconds
    if len(rms) < expected_duration:
        raise RuntimeError(
            f"expected at least {expected_duration} seconds of audio, "
            f"found {len(rms)}"
        )
    active_audio: list[float] = []
    fallback_audio: list[float] = []
    for segment in range(args.active_segments):
        start = segment * 2 * args.segment_seconds
        active_audio.extend(rms[start + 1 : start + args.segment_seconds - 1])
        if segment < args.active_segments - 1:
            fallback_start = start + args.segment_seconds
            fallback_audio.extend(
                rms[fallback_start + 1 : fallback_start + args.segment_seconds - 1]
            )
    if min(active_audio) < 1000:
        raise RuntimeError("one live audio segment is missing or too quiet")
    if max(fallback_audio) > 100:
        raise RuntimeError("one fallback audio segment is not silent")

    duration = float(
        run(
            [
                "ffprobe",
                "-v",
                "error",
                "-show_entries",
                "format=duration",
                "-of",
                "default=nw=1:nk=1",
                str(args.input),
            ]
        )
    )
    if not expected_duration - 0.1 <= duration <= expected_duration + 0.1:
        raise RuntimeError(
            f"expected a {expected_duration}-second artifact, "
            f"found {duration:.3f}s"
        )

    print(
        f"verified: {expected_frames} video frames, cadence={Counter(deltas)}, "
        f"moving unique={moving_unique}, fallback unique={fallback_unique}, "
        f"audio blocks={len(rms)}, duration={duration:.3f}s",
        flush=True,
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
