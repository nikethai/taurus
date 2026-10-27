//! HTTP transport: client setup, probing, and segmented job downloads.

pub mod client;
pub mod download;
pub mod probe;

pub use client::build_client;
pub use download::{JobContext, JobOutcome, run_job};
pub use probe::{Probe, probe};
