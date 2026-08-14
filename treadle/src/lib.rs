//! treadle — one language, two engines. See docs/treadle.md.
//!
//! The two back ends (`vm`, `tree`) must produce byte-identical `Output` for
//! every program. Their authors may not read each other's source: the oracle
//! is worth nothing if the two implementations share their mistakes.
#![forbid(unsafe_code)]

pub mod cli;
pub mod engine;
pub mod error;
pub mod output;
pub mod value;

pub mod front;
pub mod tree;
pub mod vm;
