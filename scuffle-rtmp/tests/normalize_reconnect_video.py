#!/usr/bin/env python3
"""Extract the live/fallback frame sequence from the raw POC mux output."""

from __future__ import annotations

import argparse
import hashlib
import subprocess
from pathlib import Path


def decoded_frames(source: Path, width: int, height: int):
    frame_size = width * height * 3 // 2
    command = [
        "ffmpeg",
        "-hide_banner",
        "-loglevel",
        "error",
        "-i",
        str(source),
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
    process = subprocess.Popen(command, stdout=subprocess.PIPE)
    assert process.stdout is not None
    try:
        while True:
            frame = process.stdout.read(frame_size)
            if len(frame) < frame_size:
                break
            yield frame
    finally:
        process.stdout.close()
        return_code = process.wait()
        if return_code != 0:
            raise RuntimeError(f"ffmpeg video decode failed with status {return_code}")


def motion_blocks(
    source: Path, width: int, height: int, block_size: int
) -> tuple[list[int], dict[int, int]]:
    luma_size = width * height
    block_hashes: list[str] = []
    moving: list[int] = []
    unique_by_block: dict[int, int] = {}
    for frame_number, frame in enumerate(decoded_frames(source, width, height)):
        block_hashes.append(hashlib.blake2b(frame[:luma_size], digest_size=8).hexdigest())
        if len(block_hashes) == block_size:
            block_number = frame_number // block_size
            unique = len(set(block_hashes))
            unique_by_block[block_number] = unique
            if unique >= 20:
                moving.append(block_number)
            block_hashes = []

    return moving, unique_by_block


def contiguous_runs(blocks: list[int]) -> list[tuple[int, int]]:
    if not blocks:
        return []
    runs: list[tuple[int, int]] = []
    start = previous = blocks[0]
    for block in blocks[1:]:
        if block != previous + 1:
            runs.append((start, previous))
            start = block
        previous = block
    runs.append((start, previous))
    return runs


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--input", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--width", type=int, default=320)
    parser.add_argument("--height", type=int, default=180)
    parser.add_argument("--frames-per-segment", type=int, default=300)
    parser.add_argument("--moving-segments", type=int, default=2)
    parser.add_argument("--max-boundary-motion-unique", type=int, default=3)
    args = parser.parse_args()
    if args.moving_segments < 2:
        raise RuntimeError("moving-segments must be at least 2")

    block_size = 30
    moving_blocks, unique_by_block = motion_blocks(
        args.input, args.width, args.height, block_size
    )
    runs = [run for run in contiguous_runs(moving_blocks) if run[1] - run[0] + 1 >= 8]
    if len(runs) < args.moving_segments:
        raise RuntimeError(
            f"could not find {args.moving_segments} moving runs; detected {runs}"
        )

    selected_runs = runs[: args.moving_segments]
    boundary_motion: dict[int, int] = {}
    for previous_run, next_run in zip(selected_runs, selected_runs[1:]):
        boundary_motion.update(
            {
                block: unique_by_block[block]
                for block in range(previous_run[1] + 1, next_run[0])
                if unique_by_block.get(block, 0) > args.max_boundary_motion_unique
            }
        )
    if boundary_motion:
        raise RuntimeError(
            "moving frames leaked into a fallback interval: "
            f"{boundary_motion}"
        )

    moving_starts = [run[0] * block_size for run in selected_runs]
    static_frames = [
        ((previous_run[1] + next_run[0]) // 2) * block_size + block_size // 2
        for previous_run, next_run in zip(selected_runs, selected_runs[1:])
    ]
    needed: set[int] = set(static_frames)
    for start in moving_starts:
        needed.update(range(start, start + args.frames_per_segment))

    moving_frames: list[dict[int, bytes]] = [
        {} for _ in range(args.moving_segments)
    ]
    fallback_frames: list[bytes | None] = [None] * (args.moving_segments - 1)
    for frame_number, frame in enumerate(
        decoded_frames(args.input, args.width, args.height)
    ):
        if frame_number not in needed:
            continue
        for segment, start in enumerate(moving_starts):
            if start <= frame_number < start + args.frames_per_segment:
                moving_frames[segment][frame_number] = frame
                break
        else:
            for fallback_index, static_frame in enumerate(static_frames):
                if frame_number == static_frame:
                    fallback_frames[fallback_index] = frame
                    break

    if any(
        len(frames) != args.frames_per_segment for frames in moving_frames
    ) or any(frame is None for frame in fallback_frames):
        raise RuntimeError(
            "raw video did not contain enough frames for the detected runs: "
            f"moving={[len(frames) for frames in moving_frames]}, "
            f"fallback={[frame is not None for frame in fallback_frames]}"
        )

    args.output.parent.mkdir(parents=True, exist_ok=True)
    with args.output.open("wb") as output:
        for segment, start in enumerate(moving_starts):
            for frame_number in range(start, start + args.frames_per_segment):
                output.write(moving_frames[segment][frame_number])
            if segment < len(fallback_frames):
                for _ in range(args.frames_per_segment):
                    output.write(fallback_frames[segment])

    print(
        "normalizer: moving runs "
        f"{[(run[0], run[1]) for run in selected_runs]} blocks; "
        f"fallback frames={static_frames}",
        flush=True,
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
