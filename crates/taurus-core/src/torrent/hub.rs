//! The one librqbit session an engine owns: started on first use, it adds,
//! lists, pauses and deletes torrents, applies rate limits, and reconciles
//! librqbit's own persistence with the store, which stays the source of
//! truth.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Component, Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use librqbit::api::TorrentIdOrHash;
use librqbit::dht::Id20;
use librqbit::limits::LimitsConfig;
use librqbit::{
    AddTorrent, AddTorrentOptions, AddTorrentResponse, ManagedTorrent, Session,
    ValidatedTorrentMetaV1Info,
};

use super::files::stored_output_dir;
use super::options::{SESSION_DIR, ipv6_available, limits_config, session_options};
use super::policy::classify_torrent_error;
use super::source::Source;
use super::{TorrentFile, TorrentInfo};
use crate::errors::{CoreError, ErrorKind, classify_io};
use crate::settings::TorrentSettings;
use crate::store::{JobKind, JobRecord, JobState, Store};

/// librqbit's torrent handle.
pub type TorrentHandle = Arc<ManagedTorrent>;

/// Test-only networking overrides; front ends leave this None. When set, the
/// session has no DHT, trackers, local service discovery or UPnP, listens on
/// `127.0.0.1:0`, and every torrent added gets `initial_peers`.
#[derive(Clone, Debug, Default)]
pub struct TorrentTestNet {
    pub initial_peers: Vec<SocketAddr>,
}

pub struct TorrentHub {
    /// The running session: `None` before the first start and after
    /// shutdown.
    session: Mutex<Option<Arc<Session>>>,
    /// Held while the session starts or stops, so only one start runs.
    lifecycle: tokio::sync::Mutex<()>,
    /// Set by `shutdown`; the session never starts again.
    closed: AtomicBool,
    store: Store,
    settings: TorrentSettings,
    data_dir: PathBuf,
    test: Option<TorrentTestNet>,
    /// The latest `set_limits`, as librqbit takes them; a session starting
    /// later starts with these.
    limits: Mutex<LimitsConfig>,
}

impl TorrentHub {
    /// Nothing starts until [`TorrentHub::session`] is first called.
    /// `settings` is a snapshot: the port, DHT and UPnP apply when the
    /// session starts; limits change through [`TorrentHub::set_limits`].
    /// `store` is what every start reconciles the session with.
    pub fn new(
        data_dir: &Path,
        settings: &TorrentSettings,
        test: Option<TorrentTestNet>,
        store: Store,
    ) -> TorrentHub {
        TorrentHub {
            session: Mutex::new(None),
            lifecycle: tokio::sync::Mutex::new(()),
            closed: AtomicBool::new(false),
            store,
            settings: settings.clone(),
            data_dir: data_dir.to_path_buf(),
            test,
            limits: Mutex::new(limits_config(
                settings.download_limit_bps,
                settings.upload_limit_bps,
            )),
        }
    }

    /// The session. The first call starts it and reconciles it with the
    /// store (see [`TorrentHub::reconcile`]) before anything else can use
    /// it. After [`TorrentHub::shutdown`] this is an error.
    pub async fn session(&self) -> Result<Arc<Session>, CoreError> {
        if let Some(session) = self.running()? {
            return Ok(session);
        }
        let _start = self.lifecycle.lock().await;
        if let Some(session) = self.running()? {
            return Ok(session);
        }
        let session = self.start().await?;
        *lock(&self.session) = Some(session.clone());
        // A `set_limits` while the session was starting saw no session.
        self.apply_limits(&session);
        Ok(session)
    }

    fn running(&self) -> Result<Option<Arc<Session>>, CoreError> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(CoreError {
                kind: ErrorKind::Unknown,
                detail: "the torrent session is shut down".to_string(),
            });
        }
        Ok(self.current())
    }

    fn current(&self) -> Option<Arc<Session>> {
        lock(&self.session).clone()
    }

    async fn start(&self) -> Result<Arc<Session>, CoreError> {
        let ipv6 = ipv6_available();
        let limits = *lock(&self.limits);
        let opts = session_options(
            &self.data_dir,
            &self.settings,
            self.test.is_some(),
            ipv6,
            limits,
        );
        tracing::info!(
            listen = ?opts.listen.as_ref().map(|l| l.listen_addr),
            ipv6,
            "starting the torrent session"
        );
        let session = Session::new_with_opts(self.data_dir.join(SESSION_DIR), opts)
            .await
            .map_err(lib_error)?;
        // librqbit has already restored, and started, what it persisted.
        if let Err(e) = self.reconcile_with(&session).await {
            session.cancellation_token().cancel();
            return Err(e);
        }
        Ok(session)
    }

    /// Adds a torrent from a magnet link or a `.torrent` file. A magnet
    /// resolves its metadata before this returns, so callers bound it with a
    /// timeout. A `.torrent` URL is refused: fetch it with the job's client
    /// and use [`TorrentHub::add_bytes`].
    pub async fn add(
        &self,
        source: &Source,
        output: &Path,
        only_files: Option<Vec<usize>>,
        paused: bool,
    ) -> Result<TorrentHandle, CoreError> {
        let add = add_torrent(source).await?;
        let session = self.session().await?;
        self.add_in(&session, add, output, only_files, paused).await
    }

    /// Adds a torrent from its metainfo (the bencoded `.torrent` file).
    pub async fn add_bytes(
        &self,
        torrent_meta: &[u8],
        output: &Path,
        only_files: Option<Vec<usize>>,
        paused: bool,
    ) -> Result<TorrentHandle, CoreError> {
        let session = self.session().await?;
        self.add_bytes_in(&session, torrent_meta, output, only_files, paused)
            .await
    }

    async fn add_bytes_in(
        &self,
        session: &Arc<Session>,
        torrent_meta: &[u8],
        output: &Path,
        only_files: Option<Vec<usize>>,
        paused: bool,
    ) -> Result<TorrentHandle, CoreError> {
        let add = AddTorrent::from_bytes(torrent_meta.to_vec());
        self.add_in(session, add, output, only_files, paused).await
    }

    async fn add_in(
        &self,
        session: &Arc<Session>,
        add: AddTorrent<'static>,
        output: &Path,
        only_files: Option<Vec<usize>>,
        paused: bool,
    ) -> Result<TorrentHandle, CoreError> {
        let output = output.to_str().ok_or_else(|| CoreError {
            kind: ErrorKind::FolderUnavailable,
            detail: format!("folder path is not valid UTF-8: {}", output.display()),
        })?;
        let opts = AddTorrentOptions {
            paused,
            only_files,
            // Resuming or seeding needs the files already there.
            overwrite: true,
            output_folder: Some(output.to_string()),
            ..self.add_options()
        };
        match session
            .add_torrent(add, Some(opts))
            .await
            .map_err(lib_error)?
        {
            AddTorrentResponse::Added(_, handle)
            | AddTorrentResponse::AlreadyManaged(_, handle) => Ok(handle),
            AddTorrentResponse::ListOnly(_) => Err(CoreError {
                kind: ErrorKind::Unknown,
                detail: "librqbit listed the torrent instead of adding it".to_string(),
            }),
        }
    }

    fn add_options(&self) -> AddTorrentOptions {
        AddTorrentOptions {
            initial_peers: self.test.as_ref().map(|t| t.initial_peers.clone()),
            ..Default::default()
        }
    }

    /// The torrent's metadata and metainfo bytes, without adding it. A
    /// magnet nobody serves within `timeout` is `NoPeers`. A `.torrent` URL
    /// is refused: fetch it and use [`info_from_bytes`].
    pub async fn list_files(
        &self,
        source: &Source,
        timeout: Duration,
    ) -> Result<(TorrentInfo, Vec<u8>), CoreError> {
        let add = add_torrent(source).await?;
        let opts = AddTorrentOptions {
            list_only: true,
            ..self.add_options()
        };
        let session = self.session().await?;
        let listed = tokio::time::timeout(timeout, session.add_torrent(add, Some(opts)))
            .await
            .map_err(|_| CoreError {
                kind: ErrorKind::NoPeers,
                detail: format!("no peer sent the torrent's metadata within {timeout:?}"),
            })?
            .map_err(lib_error)?;
        let AddTorrentResponse::ListOnly(listed) = listed else {
            return Err(CoreError {
                kind: ErrorKind::Unknown,
                detail: "librqbit added the torrent instead of listing it".to_string(),
            });
        };
        let fallback = match source {
            Source::Magnet { name, .. } => name.clone(),
            _ => None,
        };
        let info = torrent_info(listed.info_hash, &listed.info, fallback);
        Ok((info, listed.torrent_bytes.to_vec()))
    }

    /// Stops the torrent's transfers; pausing a paused torrent is fine.
    pub async fn pause(&self, info_hash: &str) -> Result<(), CoreError> {
        let session = self.session().await?;
        pause_in(&session, info_hash).await
    }

    /// Restarts the torrent's transfers; a running torrent is left alone.
    pub async fn unpause(&self, info_hash: &str) -> Result<(), CoreError> {
        let session = self.session().await?;
        let handle = find(&session, info_hash)?;
        if handle.live().is_some() {
            return Ok(());
        }
        session.unpause(&handle).await.map_err(lib_error)
    }

    /// Removes the torrent from the session, and with `delete_files` its own
    /// files (and its now-empty folders). A torrent the session does not
    /// have is already gone.
    pub async fn delete(&self, info_hash: &str, delete_files: bool) -> Result<(), CoreError> {
        let session = self.session().await?;
        delete_in(&session, info_hash, delete_files).await
    }

    /// The torrent's handle, if the session is running and has it.
    pub fn handle(&self, info_hash: &str) -> Option<TorrentHandle> {
        let id = parse_hash(info_hash).ok()?;
        self.current()?.get(TorrentIdOrHash::Hash(id))
    }

    /// Session-wide limits in bytes/s; `None` is unlimited. Any other value
    /// is raised to 16 KiB/s (lower stalls librqbit) and capped at
    /// `u32::MAX`.
    pub fn set_limits(&self, download_bps: Option<u64>, upload_bps: Option<u64>) {
        *lock(&self.limits) = limits_config(download_bps, upload_bps);
        if let Some(session) = self.current() {
            self.apply_limits(&session);
        }
    }

    fn apply_limits(&self, session: &Session) {
        let config = *lock(&self.limits);
        session.ratelimits.set_download_bps(config.download_bps);
        session.ratelimits.set_upload_bps(config.upload_bps);
    }

    /// Where the running session accepts peers.
    pub fn listen_addr(&self) -> Option<SocketAddr> {
        self.current()?.listen_addr()
    }

    /// Makes the session agree with the store. Every start does this; it is
    /// idempotent, so calling it again is harmless.
    /// - a session torrent with no job, or whose job is Completed or Failed,
    ///   is removed (its files stay);
    /// - one whose job may still run is paused, so only the queue starts it;
    /// - a job that may still run, has metadata and is missing from the
    ///   session is added back, paused.
    ///
    /// A torrent that cannot be fixed is logged and skipped; its job meets
    /// the error when it runs.
    pub async fn reconcile(&self) -> Result<(), CoreError> {
        let session = self.session().await?;
        self.reconcile_with(&session).await
    }

    async fn reconcile_with(&self, session: &Arc<Session>) -> Result<(), CoreError> {
        let jobs = self.store.list_jobs()?;
        let live: HashMap<&str, &JobRecord> = jobs
            .iter()
            .filter(|j| j.kind == JobKind::Torrent && may_run(j.state))
            .filter_map(|j| Some((j.info_hash.as_deref()?, j)))
            .collect();

        for handle in torrents(session) {
            let hash = handle.info_hash().as_string();
            let owned = live.contains_key(hash.as_str());
            if let Err(e) = self.reconcile_present(session, &handle, &hash, owned).await {
                tracing::warn!(info_hash = %hash, "could not reconcile a torrent: {e}");
            }
        }
        for (hash, job) in live {
            let missing =
                parse_hash(hash).is_ok_and(|id| session.get(TorrentIdOrHash::Hash(id)).is_none());
            if job.torrent_meta.is_some()
                && missing
                && let Err(e) = self.restore(session, job).await
            {
                tracing::warn!(job = job.id.0, "could not restore a torrent: {e}");
            }
        }
        Ok(())
    }

    async fn reconcile_present(
        &self,
        session: &Arc<Session>,
        handle: &TorrentHandle,
        hash: &str,
        owned: bool,
    ) -> Result<(), CoreError> {
        if !owned {
            tracing::info!(info_hash = %hash, "dropping a torrent no running job owns");
            return delete_in(session, hash, false).await;
        }
        if self.test.is_none() {
            return pause_in(session, hash).await;
        }
        // Torrents librqbit restores lose their initial peers; add them
        // again with the test peers.
        let meta = handle
            .with_metadata(|m| m.torrent_bytes.to_vec())
            .map_err(lib_error)?;
        let output = handle.output_folder().to_path_buf();
        let only_files = handle.only_files();
        delete_in(session, hash, false).await?;
        self.add_bytes_in(session, &meta, &output, only_files, true)
            .await?;
        Ok(())
    }

    async fn restore(&self, session: &Arc<Session>, job: &JobRecord) -> Result<(), CoreError> {
        // With overwrite on, a bad name could aim at a neighbour's files.
        if !is_single_name(&job.file_name) {
            return Err(CoreError {
                kind: ErrorKind::Unknown,
                detail: format!("stored name {:?} is not a plain name", job.file_name),
            });
        }
        let meta = job.torrent_meta.as_deref().unwrap_or_default();
        let info = info_from_bytes(meta)?;
        let output = stored_output_dir(&job.dest_dir, &job.file_name, &info);
        tracing::info!(job = job.id.0, output = %output.display(), "restoring a torrent");
        self.add_bytes_in(session, meta, &output, job.only_files.clone(), true)
            .await?;
        Ok(())
    }

    /// Pauses every torrent (saving that, so none comes back running on the
    /// next start), stops the session's tasks and lets go of the session.
    /// Its files close once nobody else holds a torrent handle. Every later
    /// call that needs the session fails.
    pub async fn shutdown(&self) {
        self.closed.store(true, Ordering::SeqCst);
        let _stop = self.lifecycle.lock().await;
        let Some(session) = lock(&self.session).take() else {
            return;
        };
        for handle in torrents(&session) {
            if !handle.is_paused()
                && let Err(e) = session.pause(&handle).await
            {
                tracing::warn!(info_hash = %handle.info_hash().as_string(), "could not pause a torrent: {e:#}");
            }
        }
        session.stop().await;
    }
}

impl Drop for TorrentHub {
    fn drop(&mut self) {
        // Background tasks hold the session alive; without this its listener
        // stays open after the engine is gone.
        if let Some(session) = self.current() {
            session.cancellation_token().cancel();
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

fn torrents(session: &Session) -> Vec<TorrentHandle> {
    session.with_torrents(|it| it.map(|(_, h)| h.clone()).collect())
}

async fn pause_in(session: &Session, info_hash: &str) -> Result<(), CoreError> {
    let handle = find(session, info_hash)?;
    if handle.is_paused() {
        return Ok(());
    }
    session.pause(&handle).await.map_err(lib_error)
}

async fn delete_in(
    session: &Session,
    info_hash: &str,
    delete_files: bool,
) -> Result<(), CoreError> {
    let id = parse_hash(info_hash)?;
    if session.get(TorrentIdOrHash::Hash(id)).is_none() {
        return Ok(());
    }
    session
        .delete(TorrentIdOrHash::Hash(id), delete_files)
        .await
        .map_err(lib_error)
}

/// One normal path component: not empty, `.`, `..`, rooted or nested.
fn is_single_name(name: &str) -> bool {
    !name.contains(['/', '\\'])
        && matches!(
            Path::new(name).components().collect::<Vec<_>>().as_slice(),
            [Component::Normal(_)]
        )
}

/// Jobs whose torrent belongs in the session.
fn may_run(state: JobState) -> bool {
    matches!(
        state,
        JobState::Queued
            | JobState::Waiting
            | JobState::Downloading
            | JobState::Seeding
            | JobState::Paused
    )
}

fn find(session: &Session, info_hash: &str) -> Result<TorrentHandle, CoreError> {
    session
        .get(TorrentIdOrHash::Hash(parse_hash(info_hash)?))
        .ok_or_else(|| CoreError {
            kind: ErrorKind::Unknown,
            detail: format!("torrent {info_hash} is not in the session"),
        })
}

fn parse_hash(info_hash: &str) -> Result<Id20, CoreError> {
    Id20::from_str(info_hash).map_err(|e| CoreError {
        kind: ErrorKind::Unknown,
        detail: format!("bad info hash {info_hash:?}: {e:#}"),
    })
}

/// A librqbit error, with its whole cause chain: io errors are wrapped in
/// context, so `os error N` is only in a cause.
fn lib_error(e: impl std::fmt::Display) -> CoreError {
    let detail = format!("{e:#}");
    CoreError {
        kind: classify_torrent_error(&detail),
        detail,
    }
}

async fn add_torrent(source: &Source) -> Result<AddTorrent<'static>, CoreError> {
    Ok(match source {
        Source::Magnet { uri, .. } => AddTorrent::from_url(uri.clone()),
        Source::TorrentUrl(url) => {
            return Err(CoreError {
                kind: ErrorKind::Unknown,
                detail: format!("fetch {url} first: the torrent session takes its bytes"),
            });
        }
        Source::TorrentFile(path) => {
            AddTorrent::from_bytes(tokio::fs::read(path).await.map_err(|e| CoreError {
                kind: classify_io(&e),
                detail: format!("cannot read {}: {e}", path.display()),
            })?)
        }
        Source::Http(url) => {
            return Err(CoreError {
                kind: ErrorKind::Unknown,
                detail: format!("{url} is not a torrent"),
            });
        }
    })
}

/// Parses and validates metainfo bytes.
pub fn info_from_bytes(bytes: &[u8]) -> Result<TorrentInfo, CoreError> {
    let meta = librqbit::torrent_from_bytes(bytes).map_err(lib_error)?;
    let info = meta.info.data.validate().map_err(lib_error)?;
    Ok(torrent_info(meta.info_hash, &info, None))
}

/// `fallback` names a torrent whose metadata has none (else the info hash).
fn torrent_info<B: AsRef<[u8]>>(
    info_hash: Id20,
    info: &ValidatedTorrentMetaV1Info<B>,
    fallback: Option<String>,
) -> TorrentInfo {
    let info_hash = info_hash.as_string();
    let name = info
        .name()
        .map(|n| n.into_owned())
        .filter(|n| !n.is_empty())
        .or(fallback)
        .unwrap_or_else(|| info_hash.clone());
    let files = info
        .iter_file_details()
        .enumerate()
        .map(|(index, f)| TorrentFile {
            index,
            path: f.filename.to_vec().join("/"),
            size: f.len,
        })
        .collect();
    TorrentInfo {
        info_hash,
        name,
        files,
        total: info.lengths().total_length(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stored_names_must_be_plain() {
        for bad in ["", ".", "..", "a/b", "a\\b", "/abs", "../x"] {
            assert!(!is_single_name(bad), "{bad:?}");
        }
        for good in ["Show (1)", "movie.mkv", "..a", "a.."] {
            assert!(is_single_name(good), "{good:?}");
        }
    }

    #[test]
    fn info_from_bytes_lists_files() {
        let bytes = taurus_testkit::make_torrent_bytes(
            "set",
            &[("a.bin", &[1; 40_000]), ("sub/b.bin", &[2; 5])],
            16_384,
        );
        let info = info_from_bytes(&bytes).unwrap();
        assert_eq!(info.name, "set");
        assert_eq!(info.total, 40_005);
        assert_eq!(info.info_hash.len(), 40);
        let files: Vec<_> = info
            .files
            .iter()
            .map(|f| (f.index, &*f.path, f.size))
            .collect();
        assert_eq!(files, [(0, "a.bin", 40_000), (1, "sub/b.bin", 5)]);

        let evil = taurus_testkit::make_torrent_bytes("x", &[("../e", b"boo")], 16_384);
        assert_eq!(
            info_from_bytes(&evil).unwrap_err().kind,
            ErrorKind::UnsafeTorrent
        );
    }
}
