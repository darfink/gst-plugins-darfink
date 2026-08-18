// SPDX-License-Identifier: MPL-2.0

//! Route transcribe.cpp's native diagnostics into the GStreamer debug system.
//!
//! `transcribe_cpp::init_logging()` forwards the native (and ggml) log sink to
//! the `log` crate facade. We install a `log::Log` implementation that relays
//! those records to the `transcribecpplib` debug category, so `GST_DEBUG` is
//! the single knob for everything the element emits.

use std::sync::LazyLock;

pub(crate) static LIB_CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "transcribecpplib",
        gst::DebugColorFlags::empty(),
        Some("transcribe.cpp library"),
    )
});

struct GstLogger;

impl log::Log for GstLogger {
    fn enabled(&self, _metadata: &log::Metadata) -> bool {
        true
    }

    fn log(&self, record: &log::Record) {
        let args = record.args();
        match record.level() {
            log::Level::Error => gst::error!(LIB_CAT, "{args}"),
            log::Level::Warn => gst::warning!(LIB_CAT, "{args}"),
            log::Level::Info => gst::info!(LIB_CAT, "{args}"),
            log::Level::Debug => gst::debug!(LIB_CAT, "{args}"),
            log::Level::Trace => gst::trace!(LIB_CAT, "{args}"),
        }
    }

    fn flush(&self) {}
}

/// Install the native log sink and the `log` -> GStreamer bridge.
///
/// Both installations are once-per-process. If the hosting application already
/// installed a `log` implementation we leave it alone — its logger then
/// receives the library's records instead, which is what that application
/// asked for.
pub(crate) fn init() {
    LazyLock::force(&LIB_CAT);

    transcribe_cpp::init_logging();

    static LOGGER: GstLogger = GstLogger;
    if log::set_logger(&LOGGER).is_ok() {
        log::set_max_level(log::LevelFilter::Debug);
    }
}
