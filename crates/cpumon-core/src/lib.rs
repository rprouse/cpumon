//! Core library for `cpumon`: sampling, process inspection, storage and queries.
//!
//! See `docs/design.md` for the architecture.

pub mod adapters;
pub mod duration;
pub mod error;
pub mod gpu;
pub mod paths;
mod pdh;
pub mod snapshot;

pub use error::{Error, Result};
