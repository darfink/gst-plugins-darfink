// SPDX-License-Identifier: MPL-2.0

use gst::glib;
use gst::prelude::*;

mod imp;

glib::wrapper! {
  pub struct FlvSubMux(ObjectSubclass<imp::FlvSubMux>)
    @extends gst_base::Aggregator, gst::Element, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
  gst::Element::register(
    Some(plugin),
    "flvsubmux",
    gst::Rank::NONE,
    FlvSubMux::static_type(),
  )
}
