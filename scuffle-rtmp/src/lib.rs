mod scufflertmplistensrc;

use gst::glib;

fn plugin_init(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
  scufflertmplistensrc::register(plugin)
}

gst::plugin_define!(
  scufflertmp,
  env!("CARGO_PKG_DESCRIPTION"),
  plugin_init,
  concat!(env!("CARGO_PKG_VERSION"), "-", env!("COMMIT_ID")),
  "MIT/X11",
  env!("CARGO_PKG_NAME"),
  env!("CARGO_PKG_NAME"),
  "https://crowdcast.io",
  env!("BUILD_REL_DATE")
);
