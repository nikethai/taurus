//! The external tools video downloads need (yt-dlp, ffmpeg with ffprobe,
//! Deno), which the app downloads, verifies, updates and rolls back itself.
//!
//! Layout under the data dir: `tools/<tool>/<version>/` holds a version's
//! files; `tools/<tool>/current` (a text file) names the active version;
//! the version that was active before it is kept for [`ToolManager::rollback`],
//! older ones are deleted; `tools/<tool>/.incoming/` holds a download in
//! progress. A download is accepted only if its SHA-256 matches the
//! publisher's checksum file and the installed binaries then answer their
//! version command within 10 s; otherwise it is deleted and the previous
//! version stays active. yt-dlp's own `-U` self-update is never used.

pub mod catalog;
mod install;
#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use url::Url;

use crate::errors::{CoreError, ErrorKind};
use crate::store::Store;

use catalog::asset_for;
use install::Install;

const DAY_SECS: i64 = 24 * 60 * 60;
const CURRENT_FILE: &str = "current";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize)]
pub enum ToolKind {
    YtDlp,
    Ffmpeg,
    Deno,
}

impl ToolKind {
    pub const ALL: [ToolKind; 3] = [ToolKind::YtDlp, ToolKind::Ffmpeg, ToolKind::Deno];

    /// The folder name under `tools/`, also the main binary's name (plus
    /// `.exe` on Windows).
    pub fn name(self) -> &'static str {
        match self {
            ToolKind::YtDlp => "yt-dlp",
            ToolKind::Ffmpeg => "ffmpeg",
            ToolKind::Deno => "deno",
        }
    }

    /// How often an unforced `update` asks the publisher: yt-dlp breaks as
    /// sites change, the others rarely matter.
    fn check_interval_secs(self) -> i64 {
        match self {
            ToolKind::YtDlp => DAY_SECS,
            ToolKind::Ffmpeg | ToolKind::Deno => 7 * DAY_SECS,
        }
    }

    fn index(self) -> usize {
        self as usize
    }
}

/// Where the tools are published: each field is a directory URL ending in
/// '/', onto which asset names are joined.
#[derive(Clone, Debug)]
pub struct ToolSources {
    /// yt-dlp's release assets and `SHA2-256SUMS`.
    pub yt_dlp: Url,
    /// Deno's release assets and their `.sha256sum` sidecars.
    pub deno: Url,
    /// yt-dlp's FFmpeg builds (Linux, Windows) and `checksums.sha256`.
    pub ffmpeg_builds: Url,
    /// martin-riedl.de's macOS builds: `<arm64|amd64>/release/<ffmpeg|ffprobe>.zip`
    /// redirect to the file, beside which its `.sha256` sits.
    pub ffmpeg_macos: Url,
    /// The GitHub API root, for the latest yt-dlp and Deno release tags.
    pub github_api: Url,
}

impl Default for ToolSources {
    /// The real publishers.
    fn default() -> ToolSources {
        let url = |s: &str| Url::parse(s).expect("static URL is valid");
        ToolSources {
            yt_dlp: url("https://github.com/yt-dlp/yt-dlp/releases/latest/download/"),
            deno: url("https://github.com/denoland/deno/releases/latest/download/"),
            ffmpeg_builds: url("https://github.com/yt-dlp/FFmpeg-Builds/releases/latest/download/"),
            ffmpeg_macos: url("https://ffmpeg.martin-riedl.de/redirect/latest/macos/"),
            github_api: url("https://api.github.com/"),
        }
    }
}

impl ToolSources {
    /// Every source under one directory URL `base` (ending in '/'):
    /// `yt-dlp/`, `deno/`, `ffmpeg-builds/`, `ffmpeg-macos/`, `api/`; the
    /// layout the testkit's `FakeToolHost` serves.
    pub fn under(base: &Url) -> ToolSources {
        let dir = |path: &str| base.join(path).expect("a relative path joins onto a URL");
        ToolSources {
            yt_dlp: dir("yt-dlp/"),
            deno: dir("deno/"),
            ffmpeg_builds: dir("ffmpeg-builds/"),
            ffmpeg_macos: dir("ffmpeg-macos/"),
            github_api: dir("api/"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub enum ToolState {
    Missing,
    /// A download runs; `downloaded` counts every asset of it so far.
    Installing {
        downloaded: u64,
        total: Option<u64>,
    },
    Ready,
    /// The last install or update failed. `status` reports it only while
    /// no version is installed; a failed update of an installed tool is
    /// reported (to `progress`) with the version that stays active.
    Failed(ErrorKind),
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct ToolStatus {
    pub kind: ToolKind,
    /// The active version: a release tag (yt-dlp, Deno without its `v`),
    /// or for ffmpeg, whose builds are rolling, the first 12 hex digits of
    /// its archive's SHA-256.
    pub version: Option<String>,
    /// The active main binary; ffmpeg's ffprobe sits beside it.
    pub path: Option<PathBuf>,
    pub state: ToolState,
}

/// Called with each change of a tool's status during `ensure` or `update`.
pub type Progress<'a> = &'a (dyn Fn(ToolStatus) + Send + Sync);

/// Installs, updates and rolls back the tools under `data_dir/tools`.
pub struct ToolManager {
    root: PathBuf,
    sources: ToolSources,
    client: reqwest::Client,
    /// Persists when each tool was last checked, if set.
    store: Option<Store>,
    /// Unix seconds of each tool's last check (this run).
    checked: Mutex<HashMap<ToolKind, i64>>,
    /// An install running, or the last one failed.
    states: Mutex<HashMap<ToolKind, ToolState>>,
    /// One install, update or rollback per tool at a time.
    locks: [tokio::sync::Mutex<()>; 3],
}

impl ToolManager {
    pub fn new(data_dir: &Path, sources: ToolSources, client: reqwest::Client) -> ToolManager {
        ToolManager {
            root: data_dir.join("tools"),
            sources,
            client,
            store: None,
            checked: Mutex::default(),
            states: Mutex::default(),
            locks: Default::default(),
        }
    }

    /// Keeps the check times in `store`'s settings, so the once-a-day /
    /// once-a-week rule holds across restarts. Without it they last as
    /// long as the manager.
    pub fn with_store(mut self, store: Store) -> ToolManager {
        self.store = Some(store);
        self
    }

    /// Every tool, in `ToolKind::ALL` order.
    pub fn status(&self) -> Vec<ToolStatus> {
        ToolKind::ALL.iter().map(|&k| self.status_of(k)).collect()
    }

    fn status_of(&self, kind: ToolKind) -> ToolStatus {
        let installed = self.installed(kind);
        let transient = lock(&self.states).get(&kind).cloned();
        let state = match (transient, &installed) {
            (Some(s @ ToolState::Installing { .. }), _) => s,
            (_, Some(_)) => ToolState::Ready,
            (Some(s), None) => s,
            (None, None) => ToolState::Missing,
        };
        let (version, path) = installed.unzip();
        ToolStatus {
            kind,
            version,
            path,
            state,
        }
    }

    /// The tool's main binary, installing the latest version first if none
    /// is installed. An installed tool is returned as it is, without asking
    /// the publisher. Errors: network → `ConnectionLost`; checksum mismatch
    /// → `ChecksumMismatch`; no build for this platform, or the publisher
    /// answering 404/410 → `ToolMissing`; a binary that does not answer its
    /// version command → `ToolFailed`.
    pub async fn ensure(
        &self,
        kind: ToolKind,
        progress: Progress<'_>,
    ) -> Result<PathBuf, CoreError> {
        let _guard = self.locks[kind.index()].lock().await;
        if let Some((_, path)) = self.installed(kind) {
            return Ok(path);
        }
        let installed = self.check_and_install(kind, None, progress).await?;
        Ok(installed.expect("nothing installed means something to install"))
    }

    /// Installs the publisher's latest version if it differs from the
    /// active one; `Ok(true)` if it did. Unless `force`, the publisher is
    /// asked at most once per 24 h for yt-dlp and once per 7 days for
    /// ffmpeg and Deno (an install counts as a check; a check that failed
    /// on the network does not); otherwise this returns `Ok(false)` without
    /// a request. A tool that is not installed is left alone (`Ok(false)`;
    /// see `ensure`). A failed update leaves the active version in place.
    pub async fn update(
        &self,
        kind: ToolKind,
        force: bool,
        progress: Progress<'_>,
    ) -> Result<bool, CoreError> {
        let _guard = self.locks[kind.index()].lock().await;
        let Some((version, _)) = self.installed(kind) else {
            return Ok(false);
        };
        if !force && !check_due(kind, self.checked_at(kind), unix_now()) {
            return Ok(false);
        }
        let changed = self
            .check_and_install(kind, Some(&version), progress)
            .await?;
        Ok(changed.is_some())
    }

    /// Makes the other kept version active again (the one active before the
    /// last update, or after a rollback, the one rolled back from).
    /// `ToolMissing` if there is none.
    pub fn rollback(&self, kind: ToolKind) -> Result<(), CoreError> {
        let Ok(_guard) = self.locks[kind.index()].try_lock() else {
            return Err(CoreError {
                kind: ErrorKind::Unknown,
                detail: format!("{} is being installed", kind.name()),
            });
        };
        let dir = self.tool_dir(kind);
        let current = install::read_current(&dir);
        let previous = install::other_version(&dir, current.as_deref(), &main_binary(kind));
        let Some(previous) = previous else {
            return Err(CoreError {
                kind: ErrorKind::ToolMissing,
                detail: format!("no previous version of {} to roll back to", kind.name()),
            });
        };
        install::write_current(&dir, &previous).map_err(install::io_error)?;
        lock(&self.states).remove(&kind);
        Ok(())
    }

    /// Asks the publisher for the latest version and installs it unless it
    /// is `current`. Returns the new main binary, `None` if unchanged.
    /// Records the check unless it failed on the network.
    async fn check_and_install(
        &self,
        kind: ToolKind,
        current: Option<&str>,
        progress: Progress<'_>,
    ) -> Result<Option<PathBuf>, CoreError> {
        let result = self.try_install(kind, current, progress).await;
        match &result {
            Err(e) if e.kind.is_transient() => {}
            _ => self.record_check(kind),
        }
        match &result {
            Ok(None) => {}
            Ok(Some(_)) => {
                lock(&self.states).remove(&kind);
                progress(self.status_of(kind));
            }
            Err(e) => {
                lock(&self.states).insert(kind, ToolState::Failed(e.kind));
                let mut status = self.status_of(kind);
                status.state = ToolState::Failed(e.kind);
                progress(status);
            }
        }
        result
    }

    async fn try_install(
        &self,
        kind: ToolKind,
        current: Option<&str>,
        progress: Progress<'_>,
    ) -> Result<Option<PathBuf>, CoreError> {
        let assets = asset_for(
            kind,
            std::env::consts::OS,
            std::env::consts::ARCH,
            cfg!(target_env = "musl"),
            &self.sources,
        )?;
        let install = Install {
            client: &self.client,
            dir: self.tool_dir(kind),
            assets,
            version_arg: version_arg(kind),
        };
        let tag = match kind {
            ToolKind::YtDlp => Some(
                self.latest_tag("repos/yt-dlp/yt-dlp/releases/latest")
                    .await?,
            ),
            ToolKind::Deno => Some(
                self.latest_tag("repos/denoland/deno/releases/latest")
                    .await?
                    .trim_start_matches('v')
                    .to_owned(),
            ),
            ToolKind::Ffmpeg => None,
        };
        let unchanged = match (&tag, current) {
            (_, None) => false,
            (Some(tag), Some(current)) => tag == current,
            (None, Some(current)) => install.expected_version().await? == current,
        };
        if unchanged {
            return Ok(None);
        }
        let before = self.status_of(kind);
        let report = |downloaded, total| {
            let state = ToolState::Installing { downloaded, total };
            lock(&self.states).insert(kind, state.clone());
            progress(ToolStatus {
                state,
                ..before.clone()
            });
        };
        report(0, None);
        let version = install.run(tag, current, &report).await?;
        Ok(Some(
            self.tool_dir(kind).join(version).join(main_binary(kind)),
        ))
    }

    async fn latest_tag(&self, path: &str) -> Result<String, CoreError> {
        let url = self
            .sources
            .github_api
            .join(path)
            .expect("a relative path joins onto a URL");
        install::latest_tag(&self.client, url).await
    }

    /// The active version and its main binary, if that binary exists.
    fn installed(&self, kind: ToolKind) -> Option<(String, PathBuf)> {
        let dir = self.tool_dir(kind);
        let version = install::read_current(&dir)?;
        let path = dir.join(&version).join(main_binary(kind));
        path.is_file().then_some((version, path))
    }

    fn tool_dir(&self, kind: ToolKind) -> PathBuf {
        self.root.join(kind.name())
    }

    fn checked_at(&self, kind: ToolKind) -> Option<i64> {
        if let Some(&at) = lock(&self.checked).get(&kind) {
            return Some(at);
        }
        let store = self.store.as_ref()?;
        store.tool_checked_at(kind.name()).unwrap_or_else(|e| {
            tracing::warn!("ignoring the last check time of {}: {e}", kind.name());
            None
        })
    }

    fn record_check(&self, kind: ToolKind) {
        let now = unix_now();
        lock(&self.checked).insert(kind, now);
        if let Some(store) = &self.store
            && let Err(e) = store.save_tool_checked_at(kind.name(), now)
        {
            tracing::warn!("could not save the check time of {}: {e}", kind.name());
        }
    }
}

/// Whether an unforced update of `kind` last checked at `last` (unix secs)
/// asks the publisher at `now`. A clock set back makes it due.
fn check_due(kind: ToolKind, last: Option<i64>, now: i64) -> bool {
    last.is_none_or(|last| now < last || now - last >= kind.check_interval_secs())
}

fn main_binary(kind: ToolKind) -> String {
    format!("{}{}", kind.name(), std::env::consts::EXE_SUFFIX)
}

fn version_arg(kind: ToolKind) -> &'static str {
    match kind {
        ToolKind::YtDlp | ToolKind::Deno => "--version",
        ToolKind::Ffmpeg => "-version",
    }
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // Nothing panics while holding these locks.
    mutex.lock().unwrap_or_else(|p| p.into_inner())
}
