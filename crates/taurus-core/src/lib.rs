pub mod checksum;
pub mod engine;
pub mod errors;
pub mod filename;
pub mod fixups;
pub mod grab;
pub mod http;
pub mod limit;
pub mod media;
pub mod page;
pub mod plan;
pub mod progress;
pub mod secrets;
pub mod settings;
pub mod store;
pub mod system;
pub mod timing;
pub mod tools;
pub mod torrent;
mod when_done;

pub use engine::{
    AddRequest, Engine, EngineConfig, Event, JobView, LinkInfo, MediaChoice, MediaView,
    RemoveFiles, TorrentView, WhenDoneHold,
};
pub use errors::{CoreError, ErrorKind};
pub use grab::{GrabOptions, GrabResult, GrabbedLink};
pub use secrets::Credentials;
pub use settings::{
    PowerMode, Settings, SpeedLimit, ThemeChoice, TorrentSettings, UiMode, UiSettings, WhenDone,
};
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
