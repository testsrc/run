//! Players. Each one turns "play this file" into events (see testsrc_core::check); the checker does the rest.

pub mod cmd;
pub mod ffmpeg;
pub mod gstreamer;
pub mod libvlc;
pub mod mpv;
