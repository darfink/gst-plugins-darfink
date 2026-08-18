// SPDX-License-Identifier: MPL-2.0

//! GStreamer element that injects FLV script-data subtitle tags into a muxed
//! FLV stream.

#![allow(clippy::non_send_fields_in_send_ty, unused_doc_comments)]

use gst::glib;

pub mod amf;
pub mod flv;
pub mod subinject;

fn plugin_init(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
  subinject::register(plugin)?;
  Ok(())
}

gst::plugin_define!(
  flvsubinject,
  env!("CARGO_PKG_DESCRIPTION"),
  plugin_init,
  env!("CARGO_PKG_VERSION"),
  "MPL",
  env!("CARGO_PKG_NAME"),
  env!("CARGO_PKG_NAME"),
  env!("CARGO_PKG_REPOSITORY"),
  "2026-08-11"
);
