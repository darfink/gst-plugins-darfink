#!/usr/bin/env python3
"""Probe srtsrc keep-listening=true in-band metadata per connection."""

from __future__ import annotations

import argparse
import json
import socket
import threading
import time
from dataclasses import dataclass, field
from typing import Any

import gi

gi.require_version("Gst", "1.0")
gi.require_version("GstApp", "1.0")
from gi.repository import GLib, Gst


def free_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        sock.bind(("127.0.0.1", 0))
        return int(sock.getsockname()[1])


def describe_event(event: Gst.Event) -> dict[str, Any]:
    info: dict[str, Any] = {"type": Gst.EventType.get_name(event.type)}
    if event.type == Gst.EventType.STREAM_START:
        try:
            info["stream_id"] = event.parse_stream_start()
        except Exception as e:
            info["stream_id"] = f"<parse_stream_start error: {e}>"
        try:
            has_group, group_id = event.parse_group_id()
            info["group_id"] = int(group_id) if has_group else None
            info["group_id_present"] = bool(has_group)
        except Exception as e:
            info["group_id_error"] = str(e)
        try:
            info["stream_flags"] = int(event.parse_stream_flags())
        except Exception as e:
            info["stream_flags_error"] = str(e)
    elif event.type == Gst.EventType.CAPS:
        caps = event.parse_caps()
        info["caps"] = caps.to_string() if caps else None
    elif event.type == Gst.EventType.SEGMENT:
        segment = event.parse_segment()
        info["format"] = Gst.Format.get_name(segment.format)
        info["rate"] = segment.rate
        info["start"] = int(segment.start)
        info["stop"] = int(segment.stop) if segment.stop != Gst.CLOCK_TIME_NONE else None
        info["time"] = int(segment.time)
        info["position"] = int(segment.position)
        info["offset"] = int(segment.offset)
        info["flags"] = int(segment.flags)
    elif event.type == Gst.EventType.TAG:
        tag_list = event.parse_tag()
        info["tags"] = tag_list.to_string() if tag_list else None
    elif event.type == Gst.EventType.CUSTOM_DOWNSTREAM:
        structure = event.get_structure()
        info["structure"] = structure.to_string() if structure else None
    return info


def describe_buffer(buffer: Gst.Buffer) -> dict[str, Any]:
    info: dict[str, Any] = {
        "size": int(buffer.get_size()),
        "pts": int(buffer.pts) if buffer.pts != Gst.CLOCK_TIME_NONE else None,
        "dts": int(buffer.dts) if buffer.dts != Gst.CLOCK_TIME_NONE else None,
        "duration": int(buffer.duration) if buffer.duration != Gst.CLOCK_TIME_NONE else None,
        "flags": int(buffer.mini_object.flags),
    }
    meta_names: list[str] = []
    it = buffer.iterate_meta()
    while True:
        result, meta = it.next()
        if result != Gst.IteratorResult.OK:
            break
        if meta is not None:
            meta_names.append(meta.get_api().get_name())
    if meta_names:
        info["meta"] = meta_names
    return info


@dataclass
class ConnectionLog:
    connection_id: int
    signal: str
    address: str | None = None
    events: list[dict[str, Any]] = field(default_factory=list)
    first_buffer: dict[str, Any] | None = None
    buffer_count: int = 0
    eos_seen: bool = False


class SrtsrcProbe:
    def __init__(self, port: int, payload: bytes, calls: int = 2) -> None:
        self.port = port
        self.payload = payload
        self.calls = calls
        self.loop = GLib.MainLoop()
        self.pipeline = Gst.Pipeline.new("srtsrc-probe")
        self.lock = threading.RLock()
        self.connection_counter = 0
        self.active_connection: ConnectionLog | None = None
        self.connections: list[ConnectionLog] = []
        self.errors: list[str] = []
        self.done = threading.Event()

        self.source = Gst.ElementFactory.make("srtsrc", "listener")
        if self.source is None:
            raise RuntimeError("srtsrc is unavailable")
        self.source.set_property("mode", "listener")
        self.source.set_property("localaddress", "127.0.0.1")
        self.source.set_property("localport", port)
        # uri is redundant when mode+localport are set, but keep for compat
        try:
            self.source.set_property("uri", f"srt://127.0.0.1:{port}")
        except Exception:
            pass
        self.source.set_property("keep-listening", True)
        self.source.set_property("wait-for-connection", True)
        self.source.set_property("auto-reconnect", False)
        self.source.set_property("automatic-eos", False)

        self.identity = Gst.ElementFactory.make("identity", "tap")
        if self.identity is None:
            raise RuntimeError("identity is unavailable")
        self.identity.set_property("signal-handoffs", True)
        self.identity.connect("handoff", self.on_handoff)

        self.appsink = Gst.ElementFactory.make("appsink", "sink")
        if self.appsink is None:
            raise RuntimeError("appsink is unavailable")
        self.appsink.set_property("emit-signals", True)
        self.appsink.set_property("sync", False)
        self.appsink.set_property("async", False)
        self.appsink.connect("new-sample", self.on_sample)

        self.pipeline.add(self.source)
        self.pipeline.add(self.identity)
        self.pipeline.add(self.appsink)
        if not self.source.link(self.identity):
            raise RuntimeError("failed to link srtsrc -> identity")
        if not self.identity.link(self.appsink):
            raise RuntimeError("failed to link identity -> appsink")

        src_pad = self.identity.get_static_pad("src")
        if src_pad is None:
            raise RuntimeError("identity src pad missing")
        src_pad.add_probe(Gst.PadProbeType.EVENT_DOWNSTREAM, self.on_downstream_event)

        self.source.connect("caller-added", self.on_caller_added)
        self.source.connect("caller-removed", self.on_caller_removed)

        bus = self.pipeline.get_bus()
        bus.add_signal_watch()
        bus.connect("message", self.on_bus_message)

    def log(self, message: str) -> None:
        print(message, flush=True)

    def on_caller_added(self, _element: Gst.Element, socket_id: int, address) -> None:
        with self.lock:
            self.connection_counter += 1
            addr = None
            if address is not None:
                try:
                    addr = address.to_string()
                except Exception:
                    addr = str(address)
            conn = ConnectionLog(
                connection_id=self.connection_counter,
                signal=f"caller-added socket_id={socket_id}",
                address=addr,
            )
            self.active_connection = conn
            self.connections.append(conn)
            self.log(
                f"signal connection={conn.connection_id}: caller-added "
                f"socket_id={socket_id} address={addr}"
            )

    def on_caller_removed(self, _element: Gst.Element, socket_id: int, address) -> None:
        addr = None
        if address is not None:
            try:
                addr = address.to_string()
            except Exception:
                addr = str(address)
        with self.lock:
            conn = self.active_connection
            if conn is not None:
                conn.signal += f"; caller-removed socket_id={socket_id}"
            self.log(
                f"signal connection={conn.connection_id if conn else '?'}: "
                f"caller-removed socket_id={socket_id} address={addr}"
            )
            self.active_connection = None

    def on_downstream_event(self, _pad: Gst.Pad, info: Gst.PadProbeInfo) -> Gst.PadProbeReturn:
        event = info.get_event()
        if event is None:
            return Gst.PadProbeReturn.OK
        if event.type in (
            Gst.EventType.STREAM_START,
            Gst.EventType.CAPS,
            Gst.EventType.SEGMENT,
            Gst.EventType.TAG,
            Gst.EventType.CUSTOM_DOWNSTREAM,
            Gst.EventType.EOS,
        ):
            payload = describe_event(event)
            with self.lock:
                conn = self.active_connection
                if conn is None and self.connections:
                    conn = self.connections[-1]
                if conn is not None:
                    conn.events.append(payload)
                    if event.type == Gst.EventType.EOS:
                        conn.eos_seen = True
            self.log(
                f"event connection={conn.connection_id if conn else '?'}: "
                f"{json.dumps(payload, sort_keys=True)}"
            )
        return Gst.PadProbeReturn.OK

    def on_handoff(self, _identity: Gst.Element, buffer: Gst.Buffer, _pad: Gst.Pad) -> None:
        with self.lock:
            conn = self.active_connection
            if conn is None and self.connections:
                conn = self.connections[-1]
            if conn is None:
                return
            conn.buffer_count += 1
            if conn.first_buffer is None:
                conn.first_buffer = describe_buffer(buffer)
                self.log(
                    f"buffer connection={conn.connection_id}: first "
                    f"{json.dumps(conn.first_buffer, sort_keys=True)}"
                )

    def on_sample(self, sink: Gst.Element) -> Gst.FlowReturn:
        sample = sink.emit("pull-sample")
        return Gst.FlowReturn.OK if sample is not None else Gst.FlowReturn.ERROR

    def on_bus_message(self, _bus: Gst.Bus, message: Gst.Message) -> None:
        if message.type == Gst.MessageType.ERROR:
            err, debug = message.parse_error()
            detail = f"{err}: {debug or ''}".strip()
            self.errors.append(detail)
            self.log(f"ERROR: {detail}")
            self.loop.quit()
        elif message.type == Gst.MessageType.EOS:
            self.log("pipeline: EOS")
            self.loop.quit()
        elif message.type == Gst.MessageType.ELEMENT:
            structure = message.get_structure()
            if structure is not None:
                self.log(f"element-message: {structure.to_string()}")

    def start(self) -> None:
        self.pipeline.set_state(Gst.State.PLAYING)
        threading.Thread(target=self._run_callers, daemon=True).start()

    def _run_callers(self) -> None:
        time.sleep(0.5)
        for call_idx in range(1, self.calls + 1):
            self.log(f"caller {call_idx}: connecting")
            self._send_one_call(call_idx)
            if call_idx < self.calls:
                time.sleep(0.4)
        time.sleep(0.5)
        self.pipeline.set_state(Gst.State.NULL)
        self.loop.quit()
        self.done.set()

    def _send_one_call(self, call_idx: int) -> None:
        pipeline = Gst.Pipeline.new(f"caller-{call_idx}")
        appsrc = Gst.ElementFactory.make("appsrc", "src")
        sink = Gst.ElementFactory.make("srtsink", "sink")
        if appsrc is None or sink is None:
            raise RuntimeError("caller elements unavailable")
        appsrc.set_property("is-live", True)
        appsrc.set_property("format", Gst.Format.TIME)
        sink.set_property("uri", f"srt://127.0.0.1:{self.port}")
        sink.set_property("mode", "caller")
        pipeline.add(appsrc)
        pipeline.add(sink)
        if not appsrc.link(sink):
            raise RuntimeError("failed to link caller pipeline")
        pipeline.set_state(Gst.State.PLAYING)

        chunk = self.payload
        for seq in range(3):
            buf = Gst.Buffer.new_allocate(None, len(chunk), None)
            buf.fill(0, chunk)
            buf.pts = seq * Gst.SECOND // 10
            buf.dts = buf.pts
            buf.duration = Gst.SECOND // 10
            appsrc.emit("push-buffer", buf)
            time.sleep(0.05)

        appsrc.emit("end-of-stream")
        bus = pipeline.get_bus()
        deadline = time.monotonic() + 5.0
        while time.monotonic() < deadline:
            message = bus.timed_pop_filtered(
                200 * Gst.MSECOND,
                Gst.MessageType.ERROR | Gst.MessageType.EOS,
            )
            if message is None:
                continue
            if message.type == Gst.MessageType.ERROR:
                err, debug = message.parse_error()
                raise RuntimeError(f"caller {call_idx} error: {err}: {debug}")
            if message.type == Gst.MessageType.EOS:
                break
        pipeline.set_state(Gst.State.NULL)
        self.log(f"caller {call_idx}: disconnected")

    def run(self) -> int:
        self.start()
        try:
            self.loop.run()
        finally:
            self.pipeline.set_state(Gst.State.NULL)
            self.done.wait(timeout=2.0)

        summary = {
            "port": self.port,
            "keep_listening": True,
            "connections": [
                {
                    "connection_id": c.connection_id,
                    "address": c.address,
                    "signal": c.signal,
                    "events": c.events,
                    "first_buffer": c.first_buffer,
                    "buffer_count": c.buffer_count,
                    "eos_seen": c.eos_seen,
                }
                for c in self.connections
            ],
            "errors": self.errors,
        }
        print("SUMMARY_JSON=" + json.dumps(summary, sort_keys=True), flush=True)

        if self.errors:
            return 1
        if len(self.connections) < self.calls:
            self.log(
                f"ERROR: expected {self.calls} caller-added signals, "
                f"got {len(self.connections)}"
            )
            return 1
        return 0


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--port", type=int, default=0)
    parser.add_argument("--calls", type=int, default=2)
    parser.add_argument("--payload", default="HELLO-SRT")
    args = parser.parse_args()

    Gst.init(None)
    port = args.port or free_port()
    payload = args.payload.encode("ascii")
    return SrtsrcProbe(port=port, payload=payload, calls=args.calls).run()


if __name__ == "__main__":
    raise SystemExit(main())
