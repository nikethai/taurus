//! HTTP transport: errors, probing, and segmented job downloads. The
//! client itself is built in [`crate::network`].

pub mod auth;
pub mod client;
pub mod download;
pub mod probe;
mod sources;

pub use auth::Auth;
pub use download::{JobContext, JobOutcome, host_key, run_job};
pub use probe::{Probe, probe, resolve, resolve_with};
