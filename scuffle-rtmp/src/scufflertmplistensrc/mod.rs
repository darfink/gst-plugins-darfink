mod imp;

use gst::glib;
use gst::prelude::*;

glib::wrapper! {
  pub struct ScuffleRtmpListenSrc(ObjectSubclass<imp::ScuffleRtmpListenSrc>)
    @extends gst_base::PushSrc, gst_base::BaseSrc, gst::Element, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
  gst::Element::register(
    Some(plugin),
    "scufflertmplistensrc",
    gst::Rank::NONE,
    ScuffleRtmpListenSrc::static_type(),
  )
}
