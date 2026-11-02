//! The one librqbit session an engine owns: started on first use, it adds,
//! lists, pauses and deletes torrents, applies rate limits, and reconciles
//! librqbit's own persistence with the store, which stays the source of
//! truth.

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::path::{Component, Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use librqbit::api::TorrentIdOrHash;
use librqbit::dht::Id20;
use librqbit::limits::LimitsConfig;
use librqbit::{AddTorrent, AddTorrentOptions, AddTorrentResponse, ManagedTorrent, Session};

use super::TorrentInfo;
use super::files::stored_output_dir;
pub use super::meta::info_from_bytes;
use super::meta::{add_torrent, hash_of, torrent_info};
use super::options::{SESSION_DIR, limits_config, start_session};
use super::persist::prune;
use super::policy::classify_torrent_error;
use super::source::Source;
use crate::errors::{CoreError, ErrorKind};
use crate::settings::TorrentSettings;
use crate::store::{JobKind, JobRecord, JobState, Store};

/// Starts in a row `unpause` tries, each after the files are checked.
const START_ATTEMPTS: usize = 3;
/// How long `unpause` waits for a torrent's files to be checked.
const CHECK_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// librqbit's torrent handle.
pub type TorrentHandle = Arc<ManagedTorrent>;

/// Test-only networking overrides; front ends leave this None. When set, the
/// session has no DHT, trackers, local service discovery or UPnP, listens on
/// `127.0.0.1` (on the port set, else a random one), and every torrent added
/// gets `initial_peers`.
#[derive(Clone, Debug, Default)]
pub struct TorrentTestNet {
    pub initial_peers: Vec<SocketAddr>,
    /// Torrents librqbit restores keep no initial peers, so the reconcile
    /// adds them again with these; with this set it only pauses them, as in
    /// the product (for tests whose restored torrents need no peers).
    pub restore_in_place: bool,
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
    /// A SOCKS proxy for peers and HTTP trackers (`network::torrent_proxy`).
    proxy: Option<String>,
    data_dir: PathBuf,
    test: Option<TorrentTestNet>,
    /// The latest `set_limits`, as librqbit takes them; a session starting
    /// later starts with these.
    limits: Mutex<LimitsConfig>,
    /// See [`TorrentHub::placement_lock`].
    placement: Mutex<()>,
    /// The id the next add gives its torrent. librqbit's own pick (one past
    /// the highest it saved) is the same for two adds at once, and the
    /// second then gets the first's torrent back as "already managed".
    next_id: AtomicUsize,
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
            proxy: None,
            data_dir: data_dir.to_path_buf(),
            test,
            limits: Mutex::new(limits_config(
                settings.download_limit_bps,
                settings.upload_limit_bps,
            )),
            placement: Mutex::new(()),
            next_id: AtomicUsize::new(0),
        }
    }

    /// Sets the SOCKS proxy URL (`network::torrent_proxy`) the session
    /// starts with; like the port, it is fixed once the hub exists.
    pub fn with_proxy(mut self, proxy: Option<String>) -> TorrentHub {
        self.proxy = proxy;
        self
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
        // librqbit restores (and opens the files of) what it saved before
        // the reconcile below can drop it.
        let keep: HashSet<String> = self
            .store
            .list_jobs()?
            .into_iter()
            .filter(|j| j.kind == JobKind::Torrent && may_run(j.state) && j.torrent_meta.is_some())
            .filter_map(|j| j.info_hash)
            .collect();
        prune(&self.data_dir.join(SESSION_DIR), &keep).map_err(|e| CoreError {
            kind: ErrorKind::Unknown,
            detail: format!("could not prune the torrent session's list: {e}"),
        })?;
        let limits = *lock(&self.limits);
        let session = start_session(
            &self.data_dir,
            &self.settings,
            self.proxy.as_deref(),
            self.test.is_some(),
            limits,
        )
        .await?;
        // librqbit has already restored, and started, what it persisted.
        let restored = session.with_torrents(|it| it.map(|(id, _)| id + 1).max());
        self.next_id.store(restored.unwrap_or(0), Ordering::SeqCst);
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
        let (add, info_hash) = add_torrent(source).await?;
        let session = self.session().await?;
        self.add_in(&session, add, info_hash, output, only_files, paused)
            .await
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
        let info_hash = hash_of(torrent_meta)?;
        let add = AddTorrent::from_bytes(torrent_meta.to_vec());
        self.add_in(session, add, info_hash, output, only_files, paused)
            .await
    }

    /// `info_hash` is the torrent `add` holds: what librqbit hands back must
    /// be that torrent.
    async fn add_in(
        &self,
        session: &Arc<Session>,
        add: AddTorrent<'static>,
        info_hash: Id20,
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
            preferred_id: Some(self.next_id.fetch_add(1, Ordering::SeqCst)),
            ..self.add_options()
        };
        let handle = match session
            .add_torrent(add, Some(opts))
            .await
            .map_err(lib_error)?
        {
            AddTorrentResponse::Added(_, handle)
            | AddTorrentResponse::AlreadyManaged(_, handle) => handle,
            AddTorrentResponse::ListOnly(_) => {
                return Err(CoreError {
                    kind: ErrorKind::Unknown,
                    detail: "librqbit listed the torrent instead of adding it".to_string(),
                });
            }
        };
        // "Already managed" goes by torrent id as well as by info hash.
        if handle.info_hash() != info_hash {
            return Err(CoreError {
                kind: ErrorKind::Unknown,
                detail: format!(
                    "adding torrent {} returned torrent {}",
                    info_hash.as_string(),
                    handle.info_hash().as_string()
                ),
            });
        }
        Ok(handle)
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
        let (add, _) = add_torrent(source).await?;
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
    /// A torrent whose files are being checked starts once they are; one
    /// that is still being checked after `CHECK_TIMEOUT` is a `Timeout`.
    pub async fn unpause(&self, info_hash: &str) -> Result<(), CoreError> {
        let session = self.session().await?;
        let handle = find(&session, info_hash)?;
        // During a check, librqbit's start only clears a pause request: the
        // running check then leaves the torrent paused (if it was added
        // paused), so it is started again once checked. A check a pause
        // stopped is restarted by the start, so the wait always ends; and a
        // check that stopped just as the start came ends in Error, which the
        // next start checks again.
        let mut failure = None;
        for _ in 0..START_ATTEMPTS {
            if handle.live().is_some() {
                return Ok(());
            }
            if let Err(e) = session.unpause(&handle).await {
                failure = Some(lib_error(e));
            }
            match tokio::time::timeout(CHECK_TIMEOUT, handle.wait_until_initialized()).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => failure = Some(lib_error(e)),
                Err(_) => return Err(check_timeout()),
            }
        }
        match failure {
            _ if handle.live().is_some() => Ok(()),
            Some(e) => Err(e),
            None => Err(CoreError {
                kind: ErrorKind::Unknown,
                detail: format!("torrent {info_hash} did not start"),
            }),
        }
    }

    /// Waits until the torrent's files are checked, or their check failed
    /// (which the next start meets); a torrent the session does not have
    /// has nothing to wait for. Still being checked after `CHECK_TIMEOUT`
    /// is a `Timeout`. A check that a pause stopped is not started again:
    /// that waits until the timeout.
    pub async fn wait_checked(&self, info_hash: &str) -> Result<(), CoreError> {
        let Some(handle) = self.handle(info_hash) else {
            return Ok(());
        };
        tokio::time::timeout(CHECK_TIMEOUT, handle.wait_until_initialized())
            .await
            .map(|_| ())
            .map_err(|_| check_timeout())
    }

    /// Held while a job chooses its output folder and records it, so two
    /// jobs (on any thread) never choose the same one. Never held across an
    /// await.
    pub fn placement_lock(&self) -> std::sync::MutexGuard<'_, ()> {
        lock(&self.placement)
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
        if self.test.as_ref().is_none_or(|t| t.restore_in_place) {
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

fn check_timeout() -> CoreError {
    CoreError {
        kind: ErrorKind::Timeout,
        detail: format!("the torrent's files were still being checked after {CHECK_TIMEOUT:?}"),
    }
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

pub(super) fn parse_hash(info_hash: &str) -> Result<Id20, CoreError> {
    Id20::from_str(info_hash).map_err(|e| CoreError {
        kind: ErrorKind::Unknown,
        detail: format!("bad info hash {info_hash:?}: {e:#}"),
    })
}

/// A librqbit error, with its whole cause chain: io errors are wrapped in
/// context, so `os error N` is only in a cause.
pub(super) fn lib_error(e: impl std::fmt::Display) -> CoreError {
    let detail = format!("{e:#}");
    CoreError {
        kind: classify_torrent_error(&detail),
        detail,
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
}
