// SPDX-License-Identifier: MPL-2.0

//! GStreamer element that turns word-level text into roll-up captions.

#![allow(clippy::non_send_fields_in_send_ty, unused_doc_comments)]

use gst::glib;

pub mod gapkeeper;
pub mod rollup;

fn plugin_init(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gapkeeper::register(plugin)?;
    rollup::register(plugin)?;
    Ok(())
}

gst::plugin_define!(
    textrollup,
    env!("CARGO_PKG_DESCRIPTION"),
    plugin_init,
    env!("CARGO_PKG_VERSION"),
    "MPL",
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_REPOSITORY"),
    "2026-08-06"
);
