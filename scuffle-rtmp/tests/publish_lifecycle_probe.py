#!/usr/bin/env python3
"""Verify serialized scufflertmplistensrc publisher lifecycle events."""

from __future__ import annotations

import argparse
import json
import threading
import time
from typing import Any

import gi

gi.require_version("Gst", "1.0")
from gi.repository import GLib, Gst


class PublishLifecycleProbe:
    def __init__(self, port: int, expected_publishers: int, timeout: float) -> None:
        self.port = port
        self.expected_publishers = expected_publishers
        self.timeout = timeout
        self.loop = GLib.MainLoop()
        self.pipeline = Gst.Pipeline.new("publish-lifecycle-probe")
        self.source = Gst.ElementFactory.make("scufflertmplistensrc", "listener")
        self.sink = Gst.ElementFactory.make("fakesink", "sink")
        if self.source is None or self.sink is None:
            raise RuntimeError("required GStreamer elements are unavailable")

        self.source.set_property("address", "127.0.0.1")
        self.source.set_property("port", port)
        self.source.set_property("application", "live")
        self.source.set_property("stream-key", "lifecycle")
        self.source.set_property("keep-listening", True)
        self.sink.set_property("sync", False)
        self.pipeline.add(self.source)
        self.pipeline.add(self.sink)
        if not self.source.link(self.sink):
            raise RuntimeError("failed to link listener to sink")

        self.lock = threading.RLock()
        self.sequence: list[dict[str, Any]] = []
        self.errors: list[str] = []
        self.started = 0
        self.ended = 0
        self.failed = False

        src_pad = self.source.get_static_pad("src")
        if src_pad is None:
            raise RuntimeError("listener source pad is unavailable")
        src_pad.add_probe(Gst.PadProbeType.EVENT_DOWNSTREAM, self.on_event)
        src_pad.add_probe(Gst.PadProbeType.BUFFER, self.on_buffer)

        bus = self.pipeline.get_bus()
        bus.add_signal_watch()
        bus.connect("message", self.on_bus_message)

    def on_event(self, _pad: Gst.Pad, info: Gst.PadProbeInfo) -> Gst.PadProbeReturn:
        event = info.get_event()
        if event is None:
            return Gst.PadProbeReturn.OK

        if event.type == Gst.EventType.STREAM_START:
            with self.lock:
                self.sequence.append({"kind": "stream-start"})
            print("EVENT=stream-start", flush=True)
            return Gst.PadProbeReturn.OK

        if event.type not in (
            Gst.EventType.CUSTOM_DOWNSTREAM,
            Gst.EventType.CUSTOM_DOWNSTREAM_STICKY,
        ):
            return Gst.PadProbeReturn.OK

        structure = event.get_structure()
        if structure is None:
            return Gst.PadProbeReturn.OK
        name = structure.get_name()
        if name not in (
            "scufflertmp-publish-start",
            "scufflertmp-publish-end",
        ):
            return Gst.PadProbeReturn.OK

        connection_id = int(structure.get_value("connection-id"))
        item: dict[str, Any] = {
            "kind": name,
            "connection-id": connection_id,
            "event-type": Gst.EventType.get_name(event.type),
        }
        if structure.has_field("reason"):
            item["reason"] = structure.get_value("reason")
        with self.lock:
            self.sequence.append(item)
            if name == "scufflertmp-publish-start":
                self.started += 1
            else:
                self.ended += 1
                if self.ended >= self.expected_publishers:
                    GLib.idle_add(self.loop.quit)
        print("EVENT=" + json.dumps(item, separators=(",", ":")), flush=True)
        return Gst.PadProbeReturn.OK

    def on_buffer(self, _pad: Gst.Pad, _info: Gst.PadProbeInfo) -> Gst.PadProbeReturn:
        with self.lock:
            self.sequence.append({"kind": "buffer"})
        return Gst.PadProbeReturn.OK

    def on_bus_message(self, _bus: Gst.Bus, message: Gst.Message) -> None:
        if message.type == Gst.MessageType.ERROR:
            error, debug = message.parse_error()
            detail = f"{error}: {debug or ''}".strip()
            self.errors.append(detail)
            self.failed = True
            print(f"ERROR: {detail}", flush=True)
            self.loop.quit()

    def run(self) -> int:
        self.pipeline.set_state(Gst.State.PLAYING)
        print("LISTENING", flush=True)
        GLib.timeout_add(int(self.timeout * 1000), self.on_timeout)
        try:
            self.loop.run()
        finally:
            self.pipeline.set_state(Gst.State.NULL)

        with self.lock:
            sequence = list(self.sequence)
            started = self.started
            ended = self.ended

        print("SEQUENCE=" + json.dumps(sequence, separators=(",", ":")), flush=True)
        if self.failed:
            return 1
        if started != self.expected_publishers or ended != self.expected_publishers:
            print(
                f"ERROR: expected {self.expected_publishers} starts and ends, "
                f"got {started} starts and {ended} ends",
                flush=True,
            )
            return 1

        self.validate(sequence)
        print("PASS publish lifecycle ordering", flush=True)
        return 0

    def on_timeout(self) -> bool:
        self.errors.append(f"timed out after {self.timeout:.1f}s")
        self.failed = True
        self.loop.quit()
        return False

    @staticmethod
    def validate(sequence: list[dict[str, Any]]) -> None:
        starts = [
            (index, item)
            for index, item in enumerate(sequence)
            if item["kind"] == "scufflertmp-publish-start"
        ]
        ends = [
            (index, item)
            for index, item in enumerate(sequence)
            if item["kind"] == "scufflertmp-publish-end"
        ]
        stream_starts = [
            index for index, item in enumerate(sequence) if item["kind"] == "stream-start"
        ]
        if len(stream_starts) < 2:
            raise AssertionError(
                f"expected initial and reconnect STREAM_START, got {stream_starts}"
            )

        for index, item in starts + ends:
            if item["event-type"] != "custom-downstream":
                raise AssertionError(
                    f"{item['kind']} was {item['event-type']}, expected custom-downstream"
                )

        if [item["connection-id"] for _, item in starts] != [1, 2]:
            raise AssertionError(f"unexpected start IDs: {starts}")
        if [item["connection-id"] for _, item in ends] != [1, 2]:
            raise AssertionError(f"unexpected end IDs: {ends}")

        for connection_id, (start_index, _start) in enumerate(starts, start=1):
            end_index, end = ends[connection_id - 1]
            if end_index <= start_index:
                raise AssertionError(f"publish {connection_id} ended before it started")
            if not any(
                item["kind"] == "buffer"
                for item in sequence[start_index + 1 : end_index]
            ):
                raise AssertionError(f"publish {connection_id} had no buffer")
            if end.get("reason") not in ("disconnect", "unpublished"):
                raise AssertionError(f"unexpected end reason: {end}")

        first_end = ends[0][0]
        second_stream_start = stream_starts[1]
        second_start = starts[1][0]
        if not first_end < second_stream_start < second_start:
            raise AssertionError(
                "expected publish-end(1) < reconnect STREAM_START < publish-start(2)"
            )


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--port", type=int, required=True)
    parser.add_argument("--publishers", type=int, default=2)
    parser.add_argument("--timeout", type=float, default=20.0)
    args = parser.parse_args()

    Gst.init(None)
    return PublishLifecycleProbe(args.port, args.publishers, args.timeout).run()


if __name__ == "__main__":
    raise SystemExit(main())
