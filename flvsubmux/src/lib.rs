// SPDX-License-Identifier: MPL-2.0

//! Strict, GAP-driven FLV subtitle aggregation.

#![allow(clippy::non_send_fields_in_send_ty, unused_doc_comments)]

use gst::glib;

mod amf;
mod caption;
pub mod flv;
mod submux;

fn plugin_init(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
  submux::register(plugin)
}

gst::plugin_define!(
  flvsubmux,
  env!("CARGO_PKG_DESCRIPTION"),
  plugin_init,
  env!("CARGO_PKG_VERSION"),
  "MPL",
  env!("CARGO_PKG_NAME"),
  env!("CARGO_PKG_NAME"),
  env!("CARGO_PKG_REPOSITORY"),
  "2026-08-24"
);
