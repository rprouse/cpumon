//! Core library for `cpumon`: sampling, process inspection, storage and queries.
//!
//! See `docs/design.md` for the architecture.

pub mod adapters;
pub mod console;
pub mod cpu;
pub mod duration;
pub mod error;
pub mod gpu;
pub mod hung;
pub mod incident;
pub mod offenders;
pub mod paths;
mod pdh;
pub mod procinfo;
pub mod recorder;
pub mod report;
pub mod sampler;
pub mod snapshot;
pub mod stall;
pub mod store;
pub mod task;
pub mod time;
pub mod totals;
pub mod tree;

pub use error::{Error, Result};
