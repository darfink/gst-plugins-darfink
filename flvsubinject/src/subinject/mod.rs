// SPDX-License-Identifier: MPL-2.0

use gst::glib;
use gst::prelude::*;

mod imp;

glib::wrapper! {
    pub struct FlvSubInject(ObjectSubclass<imp::FlvSubInject>) @extends gst::Element, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
  gst::Element::register(
    Some(plugin),
    "flvsubinject",
    gst::Rank::NONE,
    FlvSubInject::static_type(),
  )
}
