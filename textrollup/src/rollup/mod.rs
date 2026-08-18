// SPDX-License-Identifier: MPL-2.0

use gst::glib;
use gst::prelude::*;

mod imp;
pub mod window;

glib::wrapper! {
    pub struct TextRollup(ObjectSubclass<imp::TextRollup>) @extends gst::Element, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "textrollup",
        gst::Rank::NONE,
        TextRollup::static_type(),
    )
}
