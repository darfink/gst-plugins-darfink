#!/usr/bin/env python3
"""Reconnect proof of concept for scufflertmplistensrc.

The listener emits one FLV header for each accepted publisher.  This small
application uses that header as the demux-generation boundary. Every
generation gets a new appsrc/flvdemux/decode chain. The selectors switch
between those live branches and continuously running static-image/silence
fallback branches before feeding one persistent streamsynchronizer.
"""

from __future__ import annotations

import argparse
import threading
import time
from pathlib import Path

import gi

gi.require_version("Gst", "1.0")
gi.require_version("GstApp", "1.0")
from gi.repository import GLib, Gst


FLV_HEADER = b"FLV"


class ReconnectPoc:
    def __init__(
        self,
        port: int,
        fallback_image: Path,
        output: Path,
        expected_generations: int = 2,
        expected_disconnects: int = 2,
        selector_sync_streams: bool = True,
        selector_sync_mode: str = "active-segment",
        selector_cache_buffers: bool = False,
        output_sync: bool = False,
        mux_enforce_increasing_timestamps: bool = True,
        generation_duration_seconds: float | None = None,
        post_sync_single_segment: bool = False,
        generation_rate_no_closing_duplicates: bool = False,
        shared_video_rate: bool = False,
        shared_audio_rate: bool = False,
        preroll_before_switch: bool = False,
        normalize_sync_timestamps: bool = False,
        stop_after_final_generation: bool = False,
    ) -> None:
        if expected_generations < 1:
            raise ValueError("expected-generations must be at least 1")
        if expected_disconnects < 1:
            raise ValueError("expected-disconnects must be at least 1")
        if generation_duration_seconds is not None and generation_duration_seconds <= 0:
            raise ValueError("generation-duration-seconds must be positive")
        self.port = port
        self.fallback_image = fallback_image
        self.output = output
        self.expected_generations = expected_generations
        self.expected_disconnects = expected_disconnects
        self.selector_sync_streams = selector_sync_streams
        self.selector_sync_mode = selector_sync_mode
        self.selector_cache_buffers = selector_cache_buffers
        self.output_sync = output_sync
        self.mux_enforce_increasing_timestamps = mux_enforce_increasing_timestamps
        self.post_sync_single_segment = post_sync_single_segment
        self.generation_rate_no_closing_duplicates = generation_rate_no_closing_duplicates
        self.shared_video_rate = shared_video_rate
        self.shared_audio_rate = shared_audio_rate
        self.preroll_before_switch = preroll_before_switch
        self.normalize_sync_timestamps = normalize_sync_timestamps
        self.stop_after_final_generation = stop_after_final_generation
        self.generation_duration_ns = (
            None
            if generation_duration_seconds is None
            else round(generation_duration_seconds * Gst.SECOND)
        )
        self.loop = GLib.MainLoop()
        self.pipeline = Gst.Pipeline.new("reconnect-poc")
        self.lock = threading.RLock()
        self.current_appsrc: Gst.Element | None = None
        self.generations: list[dict] = []
        self.connection_removed: list[float] = []
        self.video_sync_timestamp_base_ns: int | None = None
        self.audio_sync_timestamp_base_ns: int | None = None
        self.observed_group_ids: set[int] = set()
        self.fallback_group_id = Gst.util_group_id_next()
        self.fallback_active = False
        self.fallback_video_timestamp_base_ns: int | None = None
        self.fallback_audio_timestamp_base_ns: int | None = None
        self.fallback_video_pad: Gst.Pad | None = None
        self.fallback_audio_pad: Gst.Pad | None = None
        self.errors: list[str] = []
        self.eos_sent = False

        self.video_selector = self.make("input-selector", "video-selector")
        self.audio_selector = self.make("input-selector", "audio-selector")
        for selector in (self.video_selector, self.audio_selector):
            selector.set_property("sync-streams", selector_sync_streams)
            selector.set_property("sync-mode", selector_sync_mode)
            selector.set_property("cache-buffers", selector_cache_buffers)
            selector.set_property("drop-backwards", True)

        if not fallback_image.is_file():
            raise FileNotFoundError(f"Fallback image does not exist: {fallback_image}")
        self.fallback_video_file = self.make("filesrc", "fallback-video-file")
        self.fallback_video_file.set_property("location", str(fallback_image))
        self.fallback_video_decoder = self.make("pngdec", "fallback-video-decoder")
        self.fallback_video_freeze = self.make("imagefreeze", "fallback-video-freeze")
        self.fallback_video_freeze.set_property("is-live", True)
        self.fallback_video_convert_scale = self.make(
            "videoconvertscale", "fallback-video-convert-scale"
        )
        self.fallback_video_valve = self.make("valve", "fallback-video-valve")
        self.fallback_video_valve.set_property("drop", True)
        self.fallback_video_valve.set_property("drop-mode", "forward-sticky-events")
        self.fallback_video_caps = self.make("capsfilter", "fallback-video-caps")
        self.fallback_video_caps.set_property(
            "caps",
            Gst.Caps.from_string(
                "video/x-raw,format=I420,width=320,height=180,framerate=30/1"
            ),
        )

        self.fallback_audio_source = self.make("audiotestsrc", "fallback-audio-source")
        self.fallback_audio_source.set_property("wave", "silence")
        self.fallback_audio_source.set_property("is-live", True)
        self.fallback_audio_convert = self.make("audioconvert", "fallback-audio-convert")
        self.fallback_audio_resample = self.make("audioresample", "fallback-audio-resample")
        self.fallback_audio_valve = self.make("valve", "fallback-audio-valve")
        self.fallback_audio_valve.set_property("drop", True)
        self.fallback_audio_valve.set_property("drop-mode", "forward-sticky-events")
        self.fallback_audio_caps = self.make("capsfilter", "fallback-audio-caps")
        self.fallback_audio_caps.set_property(
            "caps",
            Gst.Caps.from_string(
                "audio/x-raw,format=S16LE,layout=interleaved,rate=48000,channels=1"
            ),
        )

        self.streamsynchronizer = self.make("streamsynchronizer", "streamsynchronizer")
        self.video_encoder = self.make("x264enc", "final-video-encoder")
        self.video_encoder.set_property("speed-preset", "ultrafast")
        self.video_encoder.set_property("tune", "zerolatency")
        self.video_encoder.set_property("key-int-max", 30)
        self.video_encoder.set_property("bframes", 0)
        self.video_rate = self.make("videorate", "final-video-rate")
        if generation_rate_no_closing_duplicates:
            # This is the rate element that spans the selector/synchronizer
            # handoffs when shared-video-rate is enabled.  Leaving its
            # closing-segment duplication at the default can replay the last
            # frame around a reconnect even when every generation is capped
            # correctly.
            self.video_rate.set_property(
                "max-closing-segment-duplication-duration", 0
            )
        self.video_rate_caps = self.make("capsfilter", "final-video-rate-caps")
        self.video_rate_caps.set_property(
            "caps", Gst.Caps.from_string("video/x-raw,framerate=30/1")
        )
        self.video_parser = self.make("h264parse", "final-video-parser")
        self.video_sync_queue = self.make("queue", "video-sync-queue")
        self.audio_sync_queue = self.make("queue", "audio-sync-queue")
        self.video_single_segment = self.make("identity", "video-single-segment")
        self.audio_single_segment = self.make("identity", "audio-single-segment")
        for identity in (self.video_single_segment, self.audio_single_segment):
            identity.set_property("single-segment", post_sync_single_segment)
        for queue in (self.video_sync_queue, self.audio_sync_queue):
            queue.set_property("max-size-buffers", 0)
            queue.set_property("max-size-bytes", 0)
            queue.set_property("max-size-time", 0)
        self.audio_encoder = self.make("voaacenc", "final-audio-encoder")
        self.audio_encoder.set_property("bitrate", 96000)
        self.audio_timeline_convert = None
        self.audio_timeline_resample = None
        self.audio_timeline_caps = None
        self.audio_rate = None
        if shared_audio_rate:
            self.audio_timeline_convert = self.make(
                "audioconvert", "shared-audio-convert"
            )
            self.audio_timeline_resample = self.make(
                "audioresample", "shared-audio-resample"
            )
            self.audio_timeline_caps = self.make("capsfilter", "shared-audio-caps")
            self.audio_timeline_caps.set_property(
                "caps",
                Gst.Caps.from_string(
                    "audio/x-raw,format=S16LE,layout=interleaved,"
                    "rate=48000,channels=1"
                ),
            )
            self.audio_rate = self.make("audiorate", "shared-audio-rate")
        self.audio_parser = self.make("aacparse", "final-audio-parser")
        self.muxer = self.make("eflvmux", "final-muxer")
        self.muxer.set_property("streamable", True)
        self.muxer.set_property(
            "enforce-increasing-timestamps", mux_enforce_increasing_timestamps
        )
        self.filesink = self.make("filesink", "final-filesink")
        self.filesink.set_property("sync", output_sync)
        self.filesink.set_property("location", str(output))

        self.source = self.make("scufflertmplistensrc", "listener")
        self.source.set_property("address", "127.0.0.1")
        self.source.set_property("port", port)
        self.source.set_property("application", "live")
        self.source.set_property("stream-key", "reconnect")
        self.source.set_property("keep-listening", True)

        self.appsink = self.make("appsink", "raw-appsink")
        self.appsink.set_property("caps", Gst.Caps.from_string("video/x-flv"))
        self.appsink.set_property("emit-signals", True)
        self.appsink.set_property("sync", False)
        self.appsink.set_property("async", False)
        self.appsink.set_property("max-bytes", 4 * 1024 * 1024)
        self.appsink.set_property("leaky-type", "none")
        self.appsink.connect("new-preroll", self.on_preroll)
        self.appsink.connect("new-sample", self.on_sample)

        self.add_fixed_elements()
        self.streamsynchronizer.connect("pad-added", self.on_sync_pad_added)
        self.link_fixed_elements()

        bus = self.pipeline.get_bus()
        bus.add_signal_watch()
        bus.connect("message", self.on_bus_message)

    def make(self, factory: str, name: str) -> Gst.Element:
        element = Gst.ElementFactory.make(factory, name)
        if element is None:
            raise RuntimeError(f"GStreamer element is unavailable: {factory}")
        return element

    def add_fixed_elements(self) -> None:
        self.pipeline.add(
            self.source,
            self.appsink,
            self.video_selector,
            self.audio_selector,
            self.fallback_video_file,
            self.fallback_video_decoder,
            self.fallback_video_freeze,
            self.fallback_video_convert_scale,
            self.fallback_video_valve,
            self.fallback_video_caps,
            self.fallback_audio_source,
            self.fallback_audio_convert,
            self.fallback_audio_resample,
            self.fallback_audio_valve,
            self.fallback_audio_caps,
            self.streamsynchronizer,
            self.video_rate,
            self.video_rate_caps,
            self.video_encoder,
            self.video_parser,
            self.video_sync_queue,
            self.audio_sync_queue,
            self.video_single_segment,
            self.audio_single_segment,
            self.audio_encoder,
            self.audio_parser,
            self.muxer,
            self.filesink,
        )
        if self.audio_timeline_convert is not None:
            self.pipeline.add(
                self.audio_timeline_convert,
                self.audio_timeline_resample,
                self.audio_timeline_caps,
                self.audio_rate,
            )

    def link_fixed_elements(self) -> None:
        self.link_many(self.source, self.appsink)
        self.link_many(
            self.fallback_video_file,
            self.fallback_video_decoder,
            self.fallback_video_freeze,
            self.fallback_video_convert_scale,
            self.fallback_video_valve,
            self.fallback_video_caps,
        )
        self.link_many(
            self.fallback_audio_source,
            self.fallback_audio_convert,
            self.fallback_audio_resample,
            self.fallback_audio_valve,
            self.fallback_audio_caps,
        )

        self.fallback_video_pad = self.video_selector.get_request_pad("sink_%u")
        self.fallback_audio_pad = self.audio_selector.get_request_pad("sink_%u")
        if self.fallback_video_pad is None or self.fallback_audio_pad is None:
            raise RuntimeError("Could not request fallback selector pads")
        self.fallback_video_pad.set_property("always-ok", True)
        self.fallback_audio_pad.set_property("always-ok", True)
        if self.fallback_video_caps.get_static_pad("src").link(
            self.fallback_video_pad
        ) != Gst.PadLinkReturn.OK:
            raise RuntimeError("Could not link fallback video to selector")
        if self.fallback_audio_caps.get_static_pad("src").link(
            self.fallback_audio_pad
        ) != Gst.PadLinkReturn.OK:
            raise RuntimeError("Could not link fallback audio to selector")
        for pad in (
            self.fallback_video_caps.get_static_pad("src"),
            self.fallback_audio_caps.get_static_pad("src"),
        ):
            pad.add_probe(
                Gst.PadProbeType.EVENT_DOWNSTREAM,
                self.set_fixed_stream_group,
                self,
            )
        self.fallback_video_caps.get_static_pad("src").add_probe(
            Gst.PadProbeType.BUFFER,
            self.retimestamp_fallback_buffer,
            "video",
        )
        self.fallback_audio_caps.get_static_pad("src").add_probe(
            Gst.PadProbeType.BUFFER,
            self.retimestamp_fallback_buffer,
            "audio",
        )

        # Fallback becomes active only after the first disconnect.
        self.video_selector.set_property("active-pad", None)
        self.audio_selector.set_property("active-pad", None)

        video_sync_sink = self.streamsynchronizer.get_request_pad("sink_%u")
        audio_sync_sink = self.streamsynchronizer.get_request_pad("sink_%u")
        if video_sync_sink is None or audio_sync_sink is None:
            raise RuntimeError("Could not request streamsynchronizer sink pads")
        if self.video_selector.get_static_pad("src").link(video_sync_sink) != Gst.PadLinkReturn.OK:
            raise RuntimeError("Could not link video selector to streamsynchronizer")
        if self.audio_selector.get_static_pad("src").link(audio_sync_sink) != Gst.PadLinkReturn.OK:
            raise RuntimeError("Could not link audio selector to streamsynchronizer")
        self.video_selector.get_static_pad("src").add_probe(
            Gst.PadProbeType.EVENT_DOWNSTREAM,
            self.rewrite_selected_stream_group,
            "video",
        )
        self.audio_selector.get_static_pad("src").add_probe(
            Gst.PadProbeType.EVENT_DOWNSTREAM,
            self.rewrite_selected_stream_group,
            "audio",
        )
        for pad, media_type in (
            (video_sync_sink, "video"),
            (audio_sync_sink, "audio"),
        ):
            pad.add_probe(
                Gst.PadProbeType.EVENT_DOWNSTREAM,
                self.observe_stream_group,
                media_type,
            )

        self.link_many(
            self.video_single_segment,
            self.video_sync_queue,
            self.video_rate,
            self.video_rate_caps,
            self.video_encoder,
            self.video_parser,
        )
        if self.audio_timeline_convert is None:
            self.link_many(
                self.audio_single_segment,
                self.audio_sync_queue,
                self.audio_encoder,
                self.audio_parser,
            )
        else:
            self.link_many(
                self.audio_single_segment,
                self.audio_sync_queue,
                self.audio_timeline_convert,
                self.audio_timeline_resample,
                self.audio_timeline_caps,
                self.audio_rate,
                self.audio_encoder,
                self.audio_parser,
            )
        video_mux_pad = self.muxer.get_request_pad("video")
        audio_mux_pad = self.muxer.get_request_pad("audio")
        if video_mux_pad is None or audio_mux_pad is None:
            raise RuntimeError("Could not request final muxer pads")
        if not self.video_parser.get_static_pad("src").link(video_mux_pad) == Gst.PadLinkReturn.OK:
            raise RuntimeError("Could not link final video to muxer")
        if not self.audio_parser.get_static_pad("src").link(audio_mux_pad) == Gst.PadLinkReturn.OK:
            raise RuntimeError("Could not link final audio to muxer")
        if not self.muxer.link(self.filesink):
            raise RuntimeError("Could not link final muxer to filesink")

    @staticmethod
    def link_many(*elements: Gst.Element) -> None:
        for upstream, downstream in zip(elements, elements[1:]):
            if not upstream.link(downstream):
                raise RuntimeError(
                    f"Could not link {upstream.get_name()} to {downstream.get_name()}"
                )

    def on_preroll(self, sink: Gst.Element) -> Gst.FlowReturn:
        sample = sink.emit("pull-preroll")
        return self.handle_sample(sample)

    def on_sample(self, sink: Gst.Element) -> Gst.FlowReturn:
        sample = sink.emit("pull-sample")
        return self.handle_sample(sample)

    def handle_sample(self, sample: Gst.Sample | None) -> Gst.FlowReturn:
        if sample is None:
            return Gst.FlowReturn.ERROR
        buffer = sample.get_buffer()
        if buffer is None:
            return Gst.FlowReturn.ERROR

        success, mapped = buffer.map(Gst.MapFlags.READ)
        if not success:
            return Gst.FlowReturn.ERROR
        try:
            payload = bytes(mapped.data)
            is_header = payload.startswith(FLV_HEADER)
        finally:
            buffer.unmap(mapped)

        with self.lock:
            # appsink can report the initial preroll once more when it changes
            # to PLAYING.  Do not mistake that duplicate header for a second
            # publisher; a real reconnect is preceded by connection-removed.
            new_generation = is_header and (
                self.current_appsrc is None
                or len(self.connection_removed) >= len(self.generations)
            )
            if new_generation:
                self.start_generation()
            elif is_header:
                return Gst.FlowReturn.OK
            if self.current_appsrc is None:
                return Gst.FlowReturn.ERROR
            result = self.current_appsrc.emit("push-buffer", buffer.copy())
        return result

    def start_generation(self) -> None:
        generation_id = len(self.generations) + 1
        appsrc = self.make("appsrc", f"input-{generation_id}")
        demux = self.make("flvdemux", f"demux-{generation_id}")
        appsrc.set_property("caps", Gst.Caps.from_string("video/x-flv"))
        appsrc.set_property("format", Gst.Format.BYTES)
        appsrc.set_property("is-live", True)
        appsrc.set_property("block", True)
        appsrc.set_property("max-bytes", 4 * 1024 * 1024)
        appsrc.set_property("leaky-type", "none")
        appsrc.set_property("do-timestamp", False)

        generation = {
            "id": generation_id,
            "group_id": Gst.util_group_id_next(),
            "appsrc": appsrc,
            "demux": demux,
            "video": None,
            "audio": None,
            "input_eos_sent": False,
            "drained_media": set(),
            "max_duration_ns": self.generation_duration_ns,
            "passed_buffers": {"video": 0, "audio": 0},
            "dropped_buffers": {"video": 0, "audio": 0},
            "last_passed_pts": {"video": None, "audio": None},
            "ready_media": set(),
            "ready_probes": {},
            "switch_armed": False,
        }
        self.generations.append(generation)
        self.pipeline.add(appsrc, demux)
        if not appsrc.link(demux):
            raise RuntimeError(f"Could not link generation {generation_id} input")
        demux.connect("pad-added", self.on_demux_pad_added, generation)
        for element in (appsrc, demux):
            element.sync_state_with_parent()
        self.current_appsrc = appsrc
        print(f"generation {generation_id}: created new flvdemux", flush=True)

    def current_running_time_ns(self) -> int:
        clock = self.pipeline.get_clock()
        if clock is None:
            return 0
        return max(0, clock.get_time() - self.pipeline.get_base_time())

    def on_demux_pad_added(
        self, _demux: Gst.Element, pad: Gst.Pad, generation: dict
    ) -> None:
        caps = pad.get_current_caps() or pad.query_caps(None)
        if caps is None or caps.get_size() == 0:
            return
        media_type = caps.get_structure(0).get_name()
        if media_type.startswith("video/") and generation["video"] is None:
            generation["video"] = self.add_video_branch(pad, generation)
        elif media_type.startswith("audio/") and generation["audio"] is None:
            generation["audio"] = self.add_audio_branch(pad, generation)
        else:
            print(
                f"generation {generation['id']}: ignoring unexpected pad {media_type}",
                flush=True,
            )

        if generation["video"] is not None and generation["audio"] is not None:
            if self.preroll_before_switch:
                self.arm_generation_preroll(generation)
            else:
                self.activate_generation(generation)

    def arm_generation_preroll(self, generation: dict) -> None:
        if generation["switch_armed"]:
            return
        generation["switch_armed"] = True
        for media_type in ("video", "audio"):
            branch = generation[media_type]
            pad = branch["capsfilter"].get_static_pad("src")
            generation["ready_probes"][media_type] = (
                pad,
                pad.add_probe(
                    Gst.PadProbeType.BLOCK | Gst.PadProbeType.BUFFER,
                    self.on_generation_ready_buffer,
                    (generation, media_type),
                ),
            )
            branch["valve"].set_property("drop", False)

    def on_generation_ready_buffer(
        self,
        _pad: Gst.Pad,
        _info: Gst.PadProbeInfo,
        ready_data: tuple[dict, str],
    ) -> Gst.PadProbeReturn:
        generation, media_type = ready_data
        with self.lock:
            generation["ready_media"].add(media_type)
            if generation["ready_media"] == {"video", "audio"}:
                GLib.idle_add(self.activate_generation_after_preroll, generation)
        return Gst.PadProbeReturn.OK

    def activate_generation_after_preroll(self, generation: dict) -> bool:
        with self.lock:
            if generation["ready_media"] != {"video", "audio"}:
                return False
            self.activate_generation(generation)
            for pad, probe_id in generation["ready_probes"].values():
                pad.remove_probe(probe_id)
            generation["ready_probes"].clear()
        return False

    def add_video_branch(self, demux_pad: Gst.Pad, generation: dict) -> dict:
        suffix = generation["id"]
        queue = self.make("queue", f"video-queue-{suffix}")
        parser = self.make("h264parse", f"video-parser-{suffix}")
        decoder = self.make("avdec_h264", f"video-decoder-{suffix}")
        convert_scale = self.make(
            "videoconvertscale", f"video-convert-scale-{suffix}"
        )
        rate = None
        if not self.shared_video_rate:
            rate = self.make("videorate", f"video-rate-{suffix}")
            if self.generation_rate_no_closing_duplicates:
                rate.set_property("max-closing-segment-duplication-duration", 0)
        capsfilter = self.make("capsfilter", f"video-caps-{suffix}")
        valve = self.make("valve", f"video-valve-{suffix}")
        valve.set_property("drop", True)
        valve.set_property("drop-mode", "forward-sticky-events")
        capsfilter.set_property(
            "caps",
            Gst.Caps.from_string(
                "video/x-raw,format=I420,width=320,height=180"
                + ("" if self.shared_video_rate else ",framerate=30/1")
            ),
        )
        selector_pad = self.video_selector.get_request_pad("sink_%u")
        if selector_pad is None:
            raise RuntimeError("Could not request video selector pad")
        selector_pad.set_property("always-ok", True)
        elements = [valve, queue, parser, decoder, convert_scale]
        if rate is not None:
            elements.append(rate)
        elements.append(capsfilter)
        self.pipeline.add(*elements)
        self.link_many(*elements)
        if demux_pad.link(valve.get_static_pad("sink")) != Gst.PadLinkReturn.OK:
            raise RuntimeError(f"Could not link demux video pad for generation {suffix}")
        queue.get_static_pad("src").add_probe(
            Gst.PadProbeType.EVENT_DOWNSTREAM,
            self.set_stream_group,
            generation,
        )
        if capsfilter.get_static_pad("src").link(selector_pad) != Gst.PadLinkReturn.OK:
            raise RuntimeError(f"Could not link video selector for generation {suffix}")
        capsfilter.get_static_pad("src").add_probe(
            Gst.PadProbeType.EVENT_DOWNSTREAM,
            self.on_generation_event,
            (generation, "video"),
        )
        capsfilter.get_static_pad("src").add_probe(
            Gst.PadProbeType.BUFFER,
            self.trim_generation_buffer,
            (generation, "video"),
        )
        for element in elements:
            element.sync_state_with_parent()
        return {
            "elements": elements,
            "selector_pad": selector_pad,
            "valve": valve,
            "capsfilter": capsfilter,
        }

    def add_audio_branch(self, demux_pad: Gst.Pad, generation: dict) -> dict:
        suffix = generation["id"]
        queue = self.make("queue", f"audio-queue-{suffix}")
        parser = self.make("aacparse", f"audio-parser-{suffix}")
        decoder = self.make("faad", f"audio-decoder-{suffix}")
        convert = self.make("audioconvert", f"audio-convert-{suffix}")
        resample = self.make("audioresample", f"audio-resample-{suffix}")
        capsfilter = self.make("capsfilter", f"audio-caps-{suffix}")
        valve = self.make("valve", f"audio-valve-{suffix}")
        valve.set_property("drop", True)
        valve.set_property("drop-mode", "forward-sticky-events")
        capsfilter.set_property(
            "caps",
            Gst.Caps.from_string(
                "audio/x-raw,format=S16LE,layout=interleaved,rate=48000,channels=1"
            ),
        )
        selector_pad = self.audio_selector.get_request_pad("sink_%u")
        if selector_pad is None:
            raise RuntimeError("Could not request audio selector pad")
        selector_pad.set_property("always-ok", True)
        elements = (valve, queue, parser, decoder, convert, resample, capsfilter)
        self.pipeline.add(*elements)
        self.link_many(*elements)
        if demux_pad.link(valve.get_static_pad("sink")) != Gst.PadLinkReturn.OK:
            raise RuntimeError(f"Could not link demux audio pad for generation {suffix}")
        queue.get_static_pad("src").add_probe(
            Gst.PadProbeType.EVENT_DOWNSTREAM,
            self.set_stream_group,
            generation,
        )
        if capsfilter.get_static_pad("src").link(selector_pad) != Gst.PadLinkReturn.OK:
            raise RuntimeError(f"Could not link audio selector for generation {suffix}")
        capsfilter.get_static_pad("src").add_probe(
            Gst.PadProbeType.EVENT_DOWNSTREAM,
            self.on_generation_event,
            (generation, "audio"),
        )
        capsfilter.get_static_pad("src").add_probe(
            Gst.PadProbeType.BUFFER,
            self.trim_generation_buffer,
            (generation, "audio"),
        )
        for element in elements:
            element.sync_state_with_parent()
        return {
            "elements": elements,
            "selector_pad": selector_pad,
            "valve": valve,
            "capsfilter": capsfilter,
        }

    def on_generation_event(
        self,
        _pad: Gst.Pad,
        info: Gst.PadProbeInfo,
        event_data: tuple[dict, str],
    ) -> Gst.PadProbeReturn:
        event = info.get_event()
        if event is None or event.type != Gst.EventType.EOS:
            return Gst.PadProbeReturn.OK

        generation, media_type = event_data
        with self.lock:
            generation["drained_media"].add(media_type)
            should_activate_fallback = generation["drained_media"] >= {
                "video",
                "audio",
            }
        print(
            f"generation {generation['id']}: {media_type} branch drained "
            f"passed={generation['passed_buffers'][media_type]} "
            f"dropped={generation['dropped_buffers'][media_type]} "
            f"last-pts={generation['last_passed_pts'][media_type]}",
            flush=True,
        )
        if should_activate_fallback:
            # The probe runs on a branch streaming thread. Defer selector
            # changes to the GLib thread after both branches have drained.
            GLib.idle_add(self.activate_fallback_after_drain, generation)
        # Keep EOS out of the persistent selector/synchronizer. The generation
        # is complete, but the overall pipeline is intentionally not.
        return Gst.PadProbeReturn.DROP

    @staticmethod
    def trim_generation_buffer(
        _pad: Gst.Pad,
        info: Gst.PadProbeInfo,
        generation_data: tuple[dict, str],
    ) -> Gst.PadProbeReturn:
        generation, media_type = generation_data
        max_duration_ns = generation["max_duration_ns"]
        buffer = info.get_buffer()
        if buffer is None:
            return Gst.PadProbeReturn.OK
        if (
            max_duration_ns is not None
            and buffer.pts != Gst.CLOCK_TIME_NONE
            and buffer.pts >= max_duration_ns
        ):
            generation["dropped_buffers"][media_type] += 1
            return Gst.PadProbeReturn.DROP
        generation["passed_buffers"][media_type] += 1
        if buffer.pts != Gst.CLOCK_TIME_NONE:
            generation["last_passed_pts"][media_type] = buffer.pts
        return Gst.PadProbeReturn.OK

    def activate_fallback_after_drain(self, generation: dict) -> bool:
        with self.lock:
            if not self.generations or self.generations[-1] is not generation:
                return False
            if generation["drained_media"] != {"video", "audio"}:
                return False
            if (
                self.stop_after_final_generation
                and len(self.connection_removed) >= self.expected_disconnects
            ):
                # Keep the filler selected briefly so the shared videorate
                # receives a following buffer and releases the final live
                # buffer it may still hold.  Filler duration is not part of
                # the content-preservation assertion; losing a publisher
                # frame here would be.
                self.activate_fallback(final_grace=True)
                return False
            self.activate_fallback()
        return False

    @staticmethod
    def set_stream_group(
        _pad: Gst.Pad, info: Gst.PadProbeInfo, generation: dict
    ) -> Gst.PadProbeReturn:
        event = info.get_event()
        if event is not None and event.type == Gst.EventType.STREAM_START:
            replacement = event.copy()
            replacement.make_writable()
            replacement.set_group_id(generation["group_id"])
            info.set_event(replacement)
            print(
                f"generation {generation['id']}: stream-start group-id="
                f"{generation['group_id']}",
                flush=True,
            )
        return Gst.PadProbeReturn.OK

    @staticmethod
    def set_fixed_stream_group(
        _pad: Gst.Pad, info: Gst.PadProbeInfo, poc: "ReconnectPoc"
    ) -> Gst.PadProbeReturn:
        event = info.get_event()
        if event is not None and event.type == Gst.EventType.STREAM_START:
            replacement = event.copy()
            replacement.make_writable()
            replacement.set_group_id(poc.fallback_group_id)
            info.set_event(replacement)
        return Gst.PadProbeReturn.OK

    def rewrite_selected_stream_group(
        self, _pad: Gst.Pad, info: Gst.PadProbeInfo, media_type: str
    ) -> Gst.PadProbeReturn:
        event = info.get_event()
        if event is None:
            return Gst.PadProbeReturn.OK
        selector = self.video_selector if media_type == "video" else self.audio_selector
        fallback_pad = (
            self.fallback_video_pad if media_type == "video" else self.fallback_audio_pad
        )
        active_pad = selector.get_property("active-pad")
        if active_pad is None or fallback_pad is None:
            return Gst.PadProbeReturn.OK
        if active_pad.get_name() != fallback_pad.get_name():
            return Gst.PadProbeReturn.OK
        if event.type == Gst.EventType.STREAM_START:
            replacement = event.copy()
            replacement.make_writable()
            replacement.set_group_id(self.fallback_group_id)
            info.set_event(replacement)
        elif event.type == Gst.EventType.SEGMENT:
            segment = event.parse_segment()
            replacement_segment = segment.copy()
            replacement_segment.start = 0
            replacement_segment.stop = Gst.CLOCK_TIME_NONE
            replacement_segment.time = 0
            replacement_segment.base = 0
            replacement_segment.position = 0
            replacement_segment.offset = 0
            info.set_event(Gst.Event.new_segment(replacement_segment))
        return Gst.PadProbeReturn.OK

    def observe_stream_group(
        self, _pad: Gst.Pad, info: Gst.PadProbeInfo, media_type: str
    ) -> Gst.PadProbeReturn:
        event = info.get_event()
        if event is not None and event.type == Gst.EventType.STREAM_START:
            has_group_id, group_id = event.parse_group_id()
            if has_group_id:
                self.observed_group_ids.add(int(group_id))
                print(
                    f"streamsynchronizer: {media_type} stream-start "
                    f"group-id={group_id}",
                    flush=True,
                )
        return Gst.PadProbeReturn.OK

    def retimestamp_fallback_buffer(
        self, _pad: Gst.Pad, info: Gst.PadProbeInfo, media_type: str
    ) -> Gst.PadProbeReturn:
        if not self.fallback_active:
            return Gst.PadProbeReturn.OK
        buffer = info.get_buffer()
        if buffer is None or buffer.pts == Gst.CLOCK_TIME_NONE:
            return Gst.PadProbeReturn.OK
        if media_type == "video":
            base = self.fallback_video_timestamp_base_ns
        else:
            base = self.fallback_audio_timestamp_base_ns
        if base is None:
            base = buffer.pts
            if media_type == "video":
                self.fallback_video_timestamp_base_ns = base
            else:
                self.fallback_audio_timestamp_base_ns = base
        buffer = buffer.copy_deep()
        if buffer.pts >= base:
            buffer.pts -= base
        if buffer.dts != Gst.CLOCK_TIME_NONE and buffer.dts >= base:
            buffer.dts -= base
        info.set_buffer(buffer)
        return Gst.PadProbeReturn.OK

    def activate_generation(self, generation: dict) -> None:
        self.fallback_active = False
        self.fallback_video_valve.set_property("drop", True)
        self.fallback_audio_valve.set_property("drop", True)
        self.video_selector.set_property(
            "active-pad", generation["video"]["selector_pad"]
        )
        self.audio_selector.set_property(
            "active-pad", generation["audio"]["selector_pad"]
        )
        generation["video"]["valve"].set_property("drop", False)
        generation["audio"]["valve"].set_property("drop", False)
        print(f"generation {generation['id']}: switched both selectors", flush=True)

    def activate_fallback(self, final_grace: bool = False) -> None:
        if self.fallback_video_pad is None or self.fallback_audio_pad is None:
            raise RuntimeError("Fallback selector pads are not ready")
        for generation in self.generations:
            for media_type in ("video", "audio"):
                branch = generation[media_type]
                if branch is not None:
                    branch["valve"].set_property("drop", True)
        self.fallback_active = True
        self.fallback_video_timestamp_base_ns = None
        self.fallback_audio_timestamp_base_ns = None
        # Each fallback interval is a distinct segment.  This prevents the
        # persistent streamsynchronizer from treating a later disconnect as a
        # continuation of the previous fallback stream.
        self.fallback_group_id = Gst.util_group_id_next()
        self.fallback_video_valve.set_property("drop", False)
        self.fallback_audio_valve.set_property("drop", False)
        self.video_selector.set_property("active-pad", self.fallback_video_pad)
        self.audio_selector.set_property("active-pad", self.fallback_audio_pad)
        print(
            "selectors: switched to static-image/silence fallback "
            f"group-id={self.fallback_group_id}",
            flush=True,
        )
        if len(self.connection_removed) >= self.expected_disconnects:
            GLib.timeout_add(250 if final_grace else 3000, self.finish)

    def on_sync_pad_added(self, _sync: Gst.Element, pad: Gst.Pad) -> None:
        if pad.get_direction() != Gst.PadDirection.SRC:
            return
        # Requested sink_0 is video and sink_1 is audio.  The corresponding
        # src pads are created before their first caps event, so caps probing
        # here is not reliable; use the stable request order first.
        pad_name = pad.get_name()
        if pad_name == "src_0":
            media_type = "video/x-raw"
            target = self.video_single_segment.get_static_pad("sink")
            if self.normalize_sync_timestamps:
                pad.add_probe(
                    Gst.PadProbeType.BUFFER, self.normalize_video_sync_timestamp
                )
        elif pad_name == "src_1":
            media_type = "audio/x-raw"
            target = self.audio_single_segment.get_static_pad("sink")
            if self.normalize_sync_timestamps:
                pad.add_probe(
                    Gst.PadProbeType.BUFFER, self.normalize_audio_sync_timestamp
                )
        else:
            caps = pad.get_current_caps() or pad.query_caps(None)
            if caps is None or caps.get_size() == 0:
                return
            media_type = caps.get_structure(0).get_name()
            if media_type.startswith("video/"):
                target = self.video_single_segment.get_static_pad("sink")
            elif media_type.startswith("audio/"):
                target = self.audio_single_segment.get_static_pad("sink")
            else:
                return
        if pad.link(target) != Gst.PadLinkReturn.OK:
            raise RuntimeError(f"Could not link synchronizer {media_type} output")
        print(f"streamsynchronizer: linked {media_type}", flush=True)

    def normalize_video_sync_timestamp(
        self, _pad: Gst.Pad, info: Gst.PadProbeInfo
    ) -> Gst.PadProbeReturn:
        buffer = info.get_buffer()
        if buffer is None or buffer.pts == Gst.CLOCK_TIME_NONE:
            return Gst.PadProbeReturn.OK
        if self.video_sync_timestamp_base_ns is None:
            self.video_sync_timestamp_base_ns = buffer.pts
        offset = self.video_sync_timestamp_base_ns
        buffer = buffer.copy_deep()
        if buffer.pts >= offset:
            buffer.pts -= offset
        if buffer.dts != Gst.CLOCK_TIME_NONE and buffer.dts >= offset:
            buffer.dts -= offset
        info.set_buffer(buffer)
        return Gst.PadProbeReturn.OK

    def normalize_audio_sync_timestamp(
        self, _pad: Gst.Pad, info: Gst.PadProbeInfo
    ) -> Gst.PadProbeReturn:
        buffer = info.get_buffer()
        if buffer is None or buffer.pts == Gst.CLOCK_TIME_NONE:
            return Gst.PadProbeReturn.OK
        if self.audio_sync_timestamp_base_ns is None:
            self.audio_sync_timestamp_base_ns = buffer.pts
        offset = self.audio_sync_timestamp_base_ns
        buffer = buffer.copy_deep()
        if buffer.pts >= offset:
            buffer.pts -= offset
        if buffer.dts != Gst.CLOCK_TIME_NONE and buffer.dts >= offset:
            buffer.dts -= offset
        info.set_buffer(buffer)
        return Gst.PadProbeReturn.OK

    def on_bus_message(self, _bus: Gst.Bus, message: Gst.Message) -> None:
        if message.type == Gst.MessageType.ERROR:
            error, debug = message.parse_error()
            detail = f"{error}: {debug or ''}".strip()
            self.errors.append(detail)
            print(f"ERROR: {detail}", flush=True)
            self.loop.quit()
        elif message.type == Gst.MessageType.EOS:
            print("pipeline: EOS", flush=True)
            self.loop.quit()
        elif message.type == Gst.MessageType.ELEMENT:
            structure = message.get_structure()
            if structure is not None and structure.get_name() == "connection-removed":
                when = time.monotonic()
                self.connection_removed.append(when)
                with self.lock:
                    generation = self.generations[-1] if self.generations else None
                    if generation is not None and not generation["input_eos_sent"]:
                        generation["input_eos_sent"] = True
                        self.current_appsrc.emit("end-of-stream")
                        print(
                            f"generation {generation['id']}: disconnect, draining "
                            "already-received burst",
                            flush=True,
                        )
                print(
                    f"connection-removed {len(self.connection_removed)}",
                    flush=True,
                )
        elif message.type == Gst.MessageType.STATE_CHANGED and message.src == self.source:
            old, new, _pending = message.parse_state_changed()
            if new == Gst.State.PLAYING:
                print("listener: PLAYING", flush=True)

    def finish(self) -> bool:
        if self.eos_sent:
            return False
        self.eos_sent = True
        print("pipeline: stopping after final mux drain", flush=True)
        # The listener deliberately remains live after the second disconnect.
        # Sending EOS through that source cannot stop its create task while it
        # is waiting for another publisher.  A streamable FLV has no required
        # trailer, so a short drain followed by NULL cleanly closes the file.
        self.pipeline.set_state(Gst.State.NULL)
        self.loop.quit()
        return False

    def run(self) -> int:
        self.output.parent.mkdir(parents=True, exist_ok=True)
        self.pipeline.set_state(Gst.State.PLAYING)
        try:
            self.loop.run()
        finally:
            self.pipeline.set_state(Gst.State.NULL)
        if self.errors:
            return 1
        if len(self.generations) < self.expected_generations:
            print(
                "ERROR: fewer than "
                f"{self.expected_generations} demux generations were created",
                flush=True,
            )
            return 1
        if len(self.connection_removed) < self.expected_disconnects:
            print(
                "ERROR: fewer than "
                f"{self.expected_disconnects} connection-removed messages",
                flush=True,
            )
            return 1
        if len(self.observed_group_ids) < self.expected_generations:
            print(
                "ERROR: streamsynchronizer did not observe a new group ID "
                "for the reconnect",
                flush=True,
            )
            return 1
        for generation in self.generations[: self.expected_generations]:
            if generation["video"] is None or generation["audio"] is None:
                print(
                    f"ERROR: generation {generation['id']} did not produce both "
                    "video and audio branches",
                    flush=True,
                )
                return 1
        generation_group_ids = [
            generation["group_id"]
            for generation in self.generations[: self.expected_generations]
        ]
        if len(set(generation_group_ids)) != len(generation_group_ids):
            print("ERROR: reconnect generations reused a stream group ID", flush=True)
            return 1
        if not set(generation_group_ids).issubset(self.observed_group_ids):
            print(
                "ERROR: streamsynchronizer did not observe both live generation "
                "group IDs",
                flush=True,
            )
            return 1
        return 0


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--port", type=int, required=True)
    parser.add_argument("--fallback-image", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--expected-generations", type=int, default=2)
    parser.add_argument("--expected-disconnects", type=int, default=2)
    parser.add_argument(
        "--selector-sync-streams",
        type=lambda value: value.lower() in {"1", "true", "yes", "on"},
        default=True,
    )
    parser.add_argument(
        "--selector-sync-mode",
        choices=("active-segment", "clock"),
        default="active-segment",
    )
    parser.add_argument(
        "--selector-cache-buffers",
        type=lambda value: value.lower() in {"1", "true", "yes", "on"},
        default=False,
    )
    parser.add_argument(
        "--output-sync",
        type=lambda value: value.lower() in {"1", "true", "yes", "on"},
        default=False,
    )
    parser.add_argument(
        "--mux-enforce-increasing-timestamps",
        type=lambda value: value.lower() in {"1", "true", "yes", "on"},
        default=True,
    )
    parser.add_argument("--generation-duration-seconds", type=float, default=None)
    parser.add_argument(
        "--post-sync-single-segment",
        type=lambda value: value.lower() in {"1", "true", "yes", "on"},
        default=False,
    )
    parser.add_argument(
        "--generation-rate-no-closing-duplicates",
        type=lambda value: value.lower() in {"1", "true", "yes", "on"},
        default=False,
    )
    parser.add_argument(
        "--shared-video-rate",
        type=lambda value: value.lower() in {"1", "true", "yes", "on"},
        default=False,
    )
    parser.add_argument(
        "--shared-audio-rate",
        type=lambda value: value.lower() in {"1", "true", "yes", "on"},
        default=False,
    )
    parser.add_argument(
        "--preroll-before-switch",
        type=lambda value: value.lower() in {"1", "true", "yes", "on"},
        default=False,
    )
    parser.add_argument(
        "--normalize-sync-timestamps",
        type=lambda value: value.lower() in {"1", "true", "yes", "on"},
        default=False,
    )
    parser.add_argument(
        "--stop-after-final-generation",
        type=lambda value: value.lower() in {"1", "true", "yes", "on"},
        default=False,
    )
    args = parser.parse_args()
    Gst.init(None)
    try:
        return ReconnectPoc(
            args.port,
            args.fallback_image,
            args.output,
            expected_generations=args.expected_generations,
            expected_disconnects=args.expected_disconnects,
            selector_sync_streams=args.selector_sync_streams,
            selector_sync_mode=args.selector_sync_mode,
            selector_cache_buffers=args.selector_cache_buffers,
            output_sync=args.output_sync,
            mux_enforce_increasing_timestamps=args.mux_enforce_increasing_timestamps,
            generation_duration_seconds=args.generation_duration_seconds,
            post_sync_single_segment=args.post_sync_single_segment,
            generation_rate_no_closing_duplicates=args.generation_rate_no_closing_duplicates,
            shared_video_rate=args.shared_video_rate,
            shared_audio_rate=args.shared_audio_rate,
            preroll_before_switch=args.preroll_before_switch,
            normalize_sync_timestamps=args.normalize_sync_timestamps,
            stop_after_final_generation=args.stop_after_final_generation,
        ).run()
    except Exception as error:
        print(f"ERROR: {error}", flush=True)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
