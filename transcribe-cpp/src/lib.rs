// SPDX-License-Identifier: MPL-2.0

//! GStreamer speech-to-text plugin backed by [transcribe.cpp].
//!
//! [transcribe.cpp]: https://github.com/handy-computer/transcribe.cpp

// `/** */` blocks in expression position document signals for gtk-doc.
#![allow(clippy::non_send_fields_in_send_ty, unused_doc_comments)]

use gst::glib;

mod logging;
mod transcriber;

fn plugin_init(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    logging::init();
    transcriber::register(plugin)?;

    Ok(())
}

gst::plugin_define!(
    transcribecpp,
    env!("CARGO_PKG_DESCRIPTION"),
    plugin_init,
    env!("CARGO_PKG_VERSION"),
    "MPL",
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_NAME"),
    "https://github.com/handy-computer/transcribe.cpp",
    "2026-08-02"
);
