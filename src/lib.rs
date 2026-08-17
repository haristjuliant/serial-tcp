//! Bridge a serial port over TCP, on macOS, Windows and Linux.
//!
//! The whole tool is one primitive — [`bridge::bridge`] pumps bytes between two
//! endpoints — wired up two different ways. `serve` puts a serial port on one
//! side and a TCP listener on the other; `connect` puts a TCP socket on one
//! side and whatever the client wants locally on the other. That symmetry is
//! why Windows needs no special case: one half of a com0com pair is just a
//! serial port like any other.
//!
//! [`dashboard`] runs many of those bridges at once and puts a web UI in front
//! of them, without changing how any single one behaves.

pub mod bridge;
pub mod cli;
pub mod client;
pub mod dashboard;
pub mod endpoint;
pub mod list;
pub mod rfc2217;
pub mod serial;
pub mod server;
