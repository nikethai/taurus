//! The engine's plain public types: its configuration, what `add` takes,
//! the views of the jobs and the events it broadcasts.

use std::path::PathBuf;
use std::sync::Arc;

use crate::errors::ErrorKind;
use crate::media::MediaInfo;
use crate::secrets::{Credentials, KeyringStore, SecretStore};
use crate::settings::{PowerMode, Settings, WhenDone};
use crate::store::{JobId, JobKind, JobState};
use crate::system::{SpaceSource, available_space};
use crate::timing::Timing;
use crate::tools::{ToolSources, ToolStatus};
use crate::torrent::hub::TorrentTestNet;
use crate::torrent::policy::TorrentOverrides;

use super::{MediaChoice, MediaView};

pub struct EngineConfig {
    /// Holds the database, `taurus.db`, and the lock file, `taurus.lock`.
    /// Created if missing.
    pub data_dir: PathBuf,
    /// Retry and countdown delays.
    pub timing: Timing,
    /// Whether when-done actions that touch the OS really run.
    pub power: PowerMode,
    /// Whether the queue runs on its own. Without it, no job starts or is
    /// retried unless `start_now` starts it, and Waiting jobs stay Waiting
    /// on open: for short-lived front ends that only look at or edit the
    /// jobs.
    pub autostart: bool,
    /// Where yt-dlp, ffmpeg and Deno are downloaded from; the real
    /// publishers by default.
    pub tool_sources: ToolSources,
    /// Where the passwords of protected downloads are kept: the OS
    /// keychain by default. When the store fails (no keychain), the engine
    /// keeps them in memory until it stops, and a job asks again after a
    /// restart.
    pub secrets: Arc<dyn SecretStore>,
    /// Test-only: where free space is read from, so tests need not fill a
    /// disk. Not for front ends.
    #[doc(hidden)]
    pub available_space: SpaceSource,
    /// Test-only torrent networking; front ends leave this None.
    #[doc(hidden)]
    pub torrent_test: Option<TorrentTestNet>,
}

impl EngineConfig {
    /// Default timing; when-done actions run for real.
    pub fn new(data_dir: impl Into<PathBuf>) -> EngineConfig {
        EngineConfig {
            data_dir: data_dir.into(),
            timing: Timing::default(),
            power: PowerMode::Execute,
            autostart: true,
            tool_sources: ToolSources::default(),
            secrets: Arc::new(KeyringStore),
            available_space,
            torrent_test: None,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct AddRequest {
    pub url: String,
    pub dest_dir: PathBuf,
    /// Overrides the name the server suggests; sanitized before use. Web
    /// downloads only: a torrent is named after itself.
    pub file_name: Option<String>,
    /// Defaults to `Settings::connections`. Web downloads only.
    pub connections: Option<u32>,
    /// Other URLs serving the same file. Each is checked at `add`; one that
    /// fails to resolve, serves another size or another version (by its
    /// validators) is dropped.
    pub mirrors: Vec<String>,
    /// The file's expected checksum, stored with the job.
    pub checksum: Option<String>,
    /// Sent as `Referer` with every request of the job.
    pub referer: Option<String>,
    /// Torrents only: the indices of the files to download; `None` is all.
    pub only_files: Option<Vec<usize>>,
    /// Video, audio and playlist links only: the quality and conversion.
    /// `None` is the Simple default (`best`, no conversion) for a link to
    /// a well-known video site; for any other link, `Some` asks for it to
    /// be read by yt-dlp when the web server answers with a page.
    pub media: Option<MediaChoice>,
    /// Allows adding a playlist of more than 50 entries (otherwise
    /// `NeedsConfirmation`).
    pub confirm_large_playlist: bool,
    /// What `Engine::inspect` found at `url` (`LinkInfo::Media`), so a
    /// media link is not read by yt-dlp a second time; `None` reads it.
    pub media_info: Option<MediaInfo>,
    /// The user name and password of a protected (HTTP Basic auth) web
    /// download, sent only to the link's own host. The password goes to
    /// `EngineConfig::secrets`, never to the database.
    pub credentials: Option<Credentials>,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct JobView {
    pub id: JobId,
    /// The link requested for the file (after link fixups and any confirm
    /// form).
    pub url: String,
    /// The link as it was added.
    pub source_url: String,
    pub file_name: String,
    pub dest_dir: PathBuf,
    pub state: JobState,
    pub total: Option<u64>,
    pub downloaded: u64,
    /// 0 unless the job is running.
    pub speed_bps: u64,
    pub resumable: bool,
    pub error: Option<(ErrorKind, String)>,
    pub kind: JobKind,
    /// `None` for an HTTP job.
    pub torrent: Option<TorrentView>,
    /// Unix seconds of the next retry of a Waiting job; `None` otherwise.
    pub retry_at: Option<i64>,
    /// Unix seconds at which a Completed job finished; `None` otherwise.
    /// For a torrent this is when seeding ended (the download itself
    /// finished when seeding started, which the view does not carry).
    pub finished_at: Option<i64>,
    /// Lowercase hex info-hash of a torrent; `None` for an HTTP job.
    pub info_hash: Option<String>,
    /// `None` unless the job is a media job.
    pub media: Option<MediaView>,
    /// What `downloaded` and `total` count.
    pub progress_unit: ProgressUnit,
}

/// What a view's `downloaded` and `total` count.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub enum ProgressUnit {
    /// Bytes: every job but a conversion under way.
    #[default]
    Bytes,
    /// Milliseconds of media time: a convert job until it completes (a
    /// completed one counts its file's bytes).
    Millis,
}

impl ProgressUnit {
    /// The unit of a job of `kind` in `state`.
    pub fn of(kind: JobKind, state: JobState) -> ProgressUnit {
        if kind == JobKind::Convert && state != JobState::Completed {
            ProgressUnit::Millis
        } else {
            ProgressUnit::Bytes
        }
    }
}

/// What only a torrent has.
#[derive(Clone, Debug, serde::Serialize)]
pub struct TorrentView {
    /// Peers connected now; 0 unless the torrent runs.
    pub peers: u32,
    /// Bytes uploaded in all, across restarts.
    pub uploaded: u64,
    pub upload_bps: u64,
    /// Uploaded bytes over the size of the selected files.
    pub ratio: f64,
    pub files_selected: u32,
    pub files_total: u32,
    /// The torrent's own sharing rules (see `Engine::set_torrent_overrides`).
    pub overrides: TorrentOverrides,
}

#[derive(Clone, Debug, serde::Serialize)]
pub enum Event {
    JobAdded(JobView),
    JobUpdated(JobView),
    Progress {
        id: JobId,
        downloaded: u64,
        total: Option<u64>,
        speed_bps: u64,
    },
    JobRemoved(JobId),
    SettingsChanged(Settings),
    /// Nothing is Downloading, Queued or Waiting any more. Sent once each
    /// time the last such job leaves those states. The when-done action
    /// follows only when a job finishing (Completed or Failed) made the
    /// queue idle, not a pause or a removal.
    QueueIdle,
    /// A Sleep or Shutdown runs in `seconds_left` (rounded up). Sent as the
    /// countdown starts and then every second.
    WhenDoneCountdown {
        action: WhenDone,
        seconds_left: u64,
    },
    /// The countdown stopped before it ran out.
    WhenDoneCancelled,
    /// The when-done action is due: front ends carry out Notify and Quit
    /// (`command` is `None`); for Sleep and Shutdown, `command` is the
    /// command line that ran (or would have, in `PowerMode::DryRun`). Sleep
    /// and Shutdown are one-shot: `when_done` is then set back to Nothing
    /// (saved, and announced with `SettingsChanged`).
    WhenDoneFired {
        action: WhenDone,
        command: Option<String>,
    },
    /// A tool's status changed while it was installed, updated or rolled
    /// back (see `Engine::install_tools`).
    ToolStatus(ToolStatus),
}
