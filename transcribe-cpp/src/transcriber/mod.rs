// SPDX-License-Identifier: MPL-2.0

use gst::glib;
use gst::prelude::*;

mod commit;
mod imp;
mod vad;
mod worker;

/// How the element drives inference.
#[derive(Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Clone, Copy, Default, glib::Enum)]
#[repr(u32)]
#[enum_type(name = "GstTranscribeCppMode")]
#[non_exhaustive]
pub enum Mode {
    /// Pick `stream` when the model advertises streaming support, else `chunked`.
    #[default]
    #[enum_value(
        name = "Auto: stream if the model supports it, else chunked",
        nick = "auto"
    )]
    Auto = 0,
    /// Native streaming: feed every buffer, let the family commit a stable prefix.
    #[enum_value(name = "Stream: native streaming API", nick = "stream")]
    Stream = 1,
    /// Sliding-window offline inference, for families without streaming support.
    #[enum_value(name = "Chunked: sliding-window offline inference", nick = "chunked")]
    Chunked = 2,
}

/// What to do when inference cannot keep up with a live source.
#[derive(Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Clone, Copy, Default, glib::Enum)]
#[repr(u32)]
#[enum_type(name = "GstTranscribeCppOverrun")]
#[non_exhaustive]
pub enum Overrun {
    /// Block the streaming thread, propagating backpressure upstream.
    #[default]
    #[enum_value(name = "Block upstream until the worker catches up", nick = "block")]
    Block = 0,
    /// Drop the incoming audio and keep going.
    #[enum_value(name = "Drop audio that does not fit the queue", nick = "drop")]
    Drop = 1,
}

macro_rules! mirror_enum {
    (
        $(#[$meta:meta])*
        $name:ident => $target:path {
            $( $variant:ident = $value:expr, $nick:literal, $desc:literal );* $(;)?
        }
        default = $default:ident
    ) => {
        #[derive(Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Clone, Copy, glib::Enum)]
        $(#[$meta])*
        #[repr(u32)]
        #[non_exhaustive]
        pub enum $name {
            $(
                #[enum_value(name = $desc, nick = $nick)]
                $variant = $value,
            )*
        }

        impl Default for $name {
            fn default() -> Self {
                $name::$default
            }
        }

        impl From<$name> for $target {
            fn from(val: $name) -> Self {
                use $target as T;
                match val {
                    $( $name::$variant => T::$variant, )*
                }
            }
        }
    };
}

mirror_enum! {
    /// Mirrors [`transcribe_cpp::Backend`].
    #[enum_type(name = "GstTranscribeCppBackend")]
    Backend => transcribe_cpp::Backend {
        Auto = 0, "auto", "Best available device";
        Cpu = 1, "cpu", "Strict CPU, the determinism choice";
        CpuAccel = 2, "cpu-accel", "CPU plus host accelerators (BLAS/AMX)";
        Metal = 3, "metal", "Require Metal";
        Vulkan = 4, "vulkan", "Require Vulkan";
        Cuda = 5, "cuda", "Require CUDA";
    }
    default = Auto
}

mirror_enum! {
    /// Mirrors [`transcribe_cpp::Task`].
    #[enum_type(name = "GstTranscribeCppTask")]
    Task => transcribe_cpp::Task {
        Transcribe = 0, "transcribe", "Transcribe speech in its source language";
        Translate = 1, "translate", "Translate speech into the target language";
    }
    default = Transcribe
}

mirror_enum! {
    /// Mirrors [`transcribe_cpp::TimestampKind`].
    #[enum_type(name = "GstTranscribeCppTimestamps")]
    Timestamps => transcribe_cpp::TimestampKind {
        None = 0, "none", "Text only, no alignment data";
        Auto = 1, "auto", "Richest granularity the model supports";
        Segment = 2, "segment", "Segment-level start/end times";
        Word = 3, "word", "Word-level start/end times";
        Token = 4, "token", "Token-level start/end times";
    }
    default = Auto
}

mirror_enum! {
    /// Mirrors [`transcribe_cpp::Pnc`].
    #[enum_type(name = "GstTranscribeCppPnc")]
    Pnc => transcribe_cpp::Pnc {
        Default = 0, "default", "The family's shipped default";
        Off = 1, "off", "Disable punctuation and capitalization";
        On = 2, "on", "Enable punctuation and capitalization";
    }
    default = Default
}

mirror_enum! {
    /// Mirrors [`transcribe_cpp::Itn`].
    #[enum_type(name = "GstTranscribeCppItn")]
    Itn => transcribe_cpp::Itn {
        Default = 0, "default", "The family's shipped default";
        Off = 1, "off", "Disable inverse text normalization";
        On = 2, "on", "Enable inverse text normalization";
    }
    default = Default
}

mirror_enum! {
    /// Mirrors [`transcribe_cpp::KvType`].
    #[enum_type(name = "GstTranscribeCppKvType")]
    KvType => transcribe_cpp::KvType {
        Auto = 0, "auto", "f16 for quantized weights, f32 for f32 weights";
        F32 = 1, "f32", "Full-precision K/V";
        F16 = 2, "f16", "Half-precision K/V";
    }
    default = Auto
}

mirror_enum! {
    /// Mirrors [`transcribe_cpp::CommitPolicy`].
    #[enum_type(name = "GstTranscribeCppCommitPolicy")]
    CommitPolicy => transcribe_cpp::CommitPolicy {
        Auto = 0, "auto", "The family's stable-prefix implementation";
        OnFinalize = 1, "on-finalize", "Commit nothing until the stream is finalized";
        StablePrefix = 2, "stable-prefix", "Commit a raw stable prefix";
    }
    default = Auto
}

glib::wrapper! {
    pub struct Transcriber(ObjectSubclass<imp::Transcriber>) @extends gst::Element, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    #[cfg(feature = "doc")]
    {
        for ty in [
            Mode::static_type(),
            Overrun::static_type(),
            Backend::static_type(),
            Task::static_type(),
            Timestamps::static_type(),
            Pnc::static_type(),
            Itn::static_type(),
            KvType::static_type(),
            CommitPolicy::static_type(),
        ] {
            ty.mark_as_plugin_api(gst::PluginAPIFlags::empty());
        }
    }

    gst::Element::register(
        Some(plugin),
        "transcribecpptranscriber",
        gst::Rank::NONE,
        Transcriber::static_type(),
    )
}
