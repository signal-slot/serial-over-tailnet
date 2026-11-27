//! Platform-independent logic for the adapter firmware, kept separate so it can
//! be unit-tested on the host: the interactive setup shell, the line-setting
//! encodings of USB-serial chips, the reading of USB descriptors and the
//! Telnet/RFC 2217 state machine of the bridge.
//!
//! It needs only `alloc`: the ESP32 firmware uses it on std, the Pico 2 W
//! firmware without.

// The tests run on the host with std.
#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub mod rfc2217;
pub mod shell;
pub mod usbdesc;
pub mod usbip;
pub mod usbserial;
