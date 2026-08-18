// SPDX-License-Identifier: MPL-2.0

use gst::glib;
use gst::prelude::*;

mod imp;

glib::wrapper! {
    pub struct GapKeeper(ObjectSubclass<imp::GapKeeper>) @extends gst::Element, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "gapkeeper",
        gst::Rank::NONE,
        GapKeeper::static_type(),
    )
}
