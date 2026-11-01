pub mod checksum;
pub mod engine;
pub mod errors;
pub mod filename;
pub mod fixups;
pub mod http;
pub mod limit;
pub mod page;
pub mod plan;
pub mod progress;
pub mod settings;
pub mod store;
pub mod system;
pub mod timing;
pub mod torrent;
mod when_done;

pub use engine::{AddRequest, Engine, EngineConfig, Event, JobView, TorrentView, WhenDoneHold};
pub use errors::{CoreError, ErrorKind};
pub use settings::{PowerMode, Settings, SpeedLimit, TorrentSettings, WhenDone};
pub use store::{JobId, JobKind, JobState};
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
