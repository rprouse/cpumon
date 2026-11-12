//! Core library for `cpumon`: sampling, process inspection, storage and queries.
//!
//! See `docs/design.md` for the architecture.

pub mod adapters;
pub mod cpu;
pub mod duration;
pub mod error;
pub mod gpu;
pub mod paths;
mod pdh;
pub mod sampler;
pub mod snapshot;
pub mod totals;

pub use error::{Error, Result};
