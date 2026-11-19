//! testsrc frame code (layout v1), frame reader, colour and audio analysis, and the checker that compares
//! what a player showed with a case's facts. No file or process access, so it also builds for WebAssembly.
//!
//! The Python prototype (`prototype/frame-code`) is the reference: every result here is tested to be
//! identical, message text included, against the shared fixtures in `testdata/checker`.

pub mod beeps;
pub mod check;
pub mod code;
pub mod colour;
pub mod frame;
pub mod pyfmt;
pub mod reader;

pub use beeps::find_beeps;
pub use check::{check, Line, Status};
pub use code::{crc8, decode_bits, encode, Orientation};
pub use colour::{diagnose_colour, Matrix, Range};
pub use frame::{Pixels, RgbFrame, YuvFrame};
pub use reader::{find_picture, Read, Reader};
