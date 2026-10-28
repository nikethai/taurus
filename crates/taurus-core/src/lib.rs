pub mod engine;
pub mod errors;
pub mod filename;
pub mod http;
pub mod plan;
pub mod progress;
pub mod settings;
pub mod store;
pub mod timing;

pub use engine::{AddRequest, Engine, EngineConfig, Event, JobView};
pub use errors::{CoreError, ErrorKind};
pub use settings::{PowerMode, Settings, SpeedLimit, WhenDone};
pub use store::{JobId, JobState};
pub use timing::Timing;

/// Crate version, as declared in `Cargo.toml`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod tests {
    #[test]
    fn version_matches_cargo() {
        assert_eq!(super::VERSION, env!("CARGO_PKG_VERSION"));
    }
}
