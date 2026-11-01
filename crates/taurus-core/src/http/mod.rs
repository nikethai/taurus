//! HTTP transport: client setup, probing, and segmented job downloads.

pub mod client;
pub mod download;
pub mod probe;
mod sources;

pub use client::build_client;
pub use download::{JobContext, JobOutcome, host_key, run_job};
pub use probe::{Probe, probe, resolve};
