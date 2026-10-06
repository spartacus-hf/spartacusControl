//! The SCTRL LINK protocol (docs/link-protocol.md): the framed channel the
//! SPARTACUS REV1 speaks on one USB serial port once `link 1` has switched it.
//!
//! No I/O here: byte codecs, the frame model, payload parsers and builders,
//! and the screen and spectrum models the GUI draws from. The firmware's
//! host suite exports the vectors these are tested against
//! (`tests/fixtures/link_vectors.txt`).

pub mod crc;
pub mod frame;
pub mod payload;
pub mod rle;
pub mod screen;
pub mod slip;
pub mod spectrum;

pub use frame::{Frame, HostType, RadioType};
pub use slip::{Decoder, encode_frame};
