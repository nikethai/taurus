//! The public entry point: a cheap-to-clone [`Engine`] handle over one
//! background actor that owns the running jobs, restores jobs from the store
//! on open and broadcasts [`Event`]s.
//!
//! Network probes run in the caller's task; everything that touches the
//! running jobs or reserves names runs in the actor, one command at a time.

mod actor;
mod convert;
mod convert_guard;
mod convert_job;
mod cookies;
mod detail;
mod grab;
mod inspect;
mod jobs;
mod keychain;
mod media;
mod media_job;
mod media_rate;
mod mirrors;
mod open;
mod remove;
mod schedule;
mod settings;
mod sign_in;
mod stop_requests;
mod tool_ops;
mod torrent;
mod torrent_edit;
mod types;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::Duration;

use tokio::sync::{broadcast, mpsc, oneshot};

use crate::checksum;
use crate::errors::{CoreError, ErrorKind, classify_io};
use crate::http::{Auth, resolve_with};
use crate::media::fast_path_host;
use crate::network::SharedClient;
use crate::settings::Settings;
use crate::store::{JobId, JobKind, JobState, Store};
use crate::system::SpaceSource;
use crate::tools::{ToolManager, ToolStatus};
use crate::torrent::hub::TorrentHub;
use crate::torrent::source::{Source, classify};

use actor::Command;
#[doc(hidden)]
pub use actor::JobMsg;
use detail::Details;
pub use detail::{JobDetail, SegmentView, TorrentFileProgress};
pub use inspect::LinkInfo;
use jobs::{downloaded, fixed_url, view_of};
use keychain::Secrets;
pub use media::{MediaChoice, MediaView};
use mirrors::vet_mirrors;
use open::assemble;
pub use remove::RemoveFiles;
use schedule::check_space;
use sign_in::web_only;
pub use types::{AddRequest, EngineConfig, Event, JobView, ProgressUnit, TorrentView};

const COMMAND_CAPACITY: usize = 64;
const EVENT_CAPACITY: usize = 1024;
const DB_FILE: &str = "taurus.db";
const LOCK_FILE: &str = "taurus.lock";
/// Start of the detail of the error `Engine::open` returns when another
/// engine holds the data dir.
const IN_USE: &str = "another taurus instance is using";

type Views = Arc<RwLock<HashMap<JobId, JobView>>>;

/// Handle to the download engine. Clones share one engine.
#[derive(Clone)]
pub struct Engine {
    commands: mpsc::Sender<Command>,
    events: broadcast::Sender<Event>,
    views: Views,
    /// The actor's settings, copied for `settings`.
    settings: Arc<RwLock<Settings>>,
    /// The actor's keep-awake wish, copied for `keeping_awake`.
    keeping_awake: Arc<AtomicBool>,
    /// The HTTP client, shared with the actor and the tools; take it
    /// (`get`) for each request.
    client: SharedClient,
    /// Live `WhenDoneHold`s, shared with the actor.
    holds: Arc<AtomicUsize>,
    /// Tells the actor that `holds` changed.
    hold_tx: mpsc::UnboundedSender<JobMsg>,
    space: SpaceSource,
    /// The engine's torrent session, shared with the actor.
    hub: Arc<TorrentHub>,
    /// How long a magnet link's metadata is waited for.
    metadata_timeout: Duration,
    /// yt-dlp, ffmpeg and Deno under `data_dir/tools`.
    tools: Arc<ToolManager>,
    /// Holds the database, the tools and the media jobs' thumbnails.
    data_dir: PathBuf,
    /// The jobs' passwords, shared with the actor.
    secrets: Secrets,
    /// What `job_detail` reads beside the views, shared with the actor.
    details: Details,
}

impl Engine {
    /// Opens the database in `config.data_dir`, restores its jobs, applies
    /// the saved settings (as much of them as reads; see
    /// `Store::load_settings_lenient`) and starts the queue. Jobs recorded as Downloading
    /// (interrupted by a crash, kill or reboot) become Queued, keeping their
    /// place in the queue and their progress; before such a job runs again
    /// its link is resolved again and checked like `resume` does (restarting
    /// from zero if the file changed). Waiting jobs are queued again
    /// at once, with Queued ones. Without `config.autostart`, nothing starts
    /// and Waiting jobs are left as stored.
    ///
    /// The engine holds `data_dir` exclusively until it shuts down or its
    /// last handle is dropped. While another engine (in any process) holds
    /// it, `open` fails without writing anything and the error's
    /// `is_instance_locked` is true; `read_snapshot` still works.
    pub async fn open(config: EngineConfig) -> Result<Engine, CoreError> {
        let (engine, mut actor, commands) = assemble(config).await?;
        actor.after_change();
        tokio::spawn(actor.run(commands));
        stop_requests::watch(&engine.data_dir, &engine.commands);
        Ok(engine)
    }

    /// The tools' versions and paths as stored in `config.data_dir`, read
    /// without opening an engine, so it works while another engine holds the
    /// data dir. Every tool is Ready or Missing: an install in progress in
    /// another process is not visible.
    pub fn read_tools_status(config: &EngineConfig) -> Vec<ToolStatus> {
        ToolManager::read_status(&config.data_dir)
    }

    /// Every stored job, ordered by id, read without opening an engine or
    /// writing anything, so it works while another engine holds `data_dir`.
    /// Views show the stored state (a running job is Downloading) and the
    /// durable progress. No database yet means no jobs.
    pub fn read_snapshot(config: &EngineConfig) -> Result<Vec<JobView>, CoreError> {
        let Some(store) = Store::open_read_only(&config.data_dir.join(DB_FILE))? else {
            return Ok(Vec::new());
        };
        store
            .list_jobs()?
            .iter()
            .map(|job| Ok(view_of(&store, job, downloaded(&store, job.id)?)))
            .collect()
    }

    /// Resolves the URL (following a confirm page; failing on any other web
    /// page or bot check), keeps the mirrors that serve the same file,
    /// reserves a file name that no other file or job in `dest_dir` uses
    /// (ignoring case), and queues the job; it starts once fewer than
    /// `max_active` jobs download. The job records `dest_dir` canonicalized
    /// (symlinks and `.` resolved). A missing `dest_dir` fails with
    /// `FolderUnavailable`, a file that does not fit on its disk (beside
    /// what the running jobs on that disk still download) with `DiskFull`
    /// or `FileTooLargeForDisk`, a checksum that is not a SHA-256
    /// with `Unknown`; nothing is recorded when `add` fails.
    ///
    /// A magnet link, `.torrent` URL (fetched with the job's Referer) or
    /// local `.torrent` file is queued as a torrent job, named after the
    /// torrent, unless a job already has that torrent (`AlreadyAdded`); it
    /// starts once fewer than `torrent.max_active` torrents download.
    /// `only_files` selects its files; mirrors and a checksum are refused.
    ///
    /// A link to a well-known video site (or, with `req.media` set, any web
    /// page yt-dlp reads) is read by yt-dlp (see `inspect`) and queued as a
    /// media job named after the video (or playlist), with `req.media`'s
    /// quality and conversion (`best` and none by default). A playlist of
    /// more than 50 entries is refused with `NeedsConfirmation` and the
    /// detail `playlist:<count>` unless `confirm_large_playlist`; a single
    /// video whose estimated size does not fit fails like a file; mirrors
    /// and a checksum are refused. With `req.media_info` the link is taken
    /// as the media it describes, and not read again. It counts toward `max_active` like a web
    /// download; see `stop_recording` for live streams.
    ///
    /// With `req.credentials` a web download signs in (HTTP Basic auth) on
    /// its link's own origin only, mirrors there included, and on that
    /// host's https when an http link is upgraded (see `http::Auth`). The
    /// password goes to `EngineConfig::secrets` and is kept there once
    /// `add` returns. A 401 fails with `NeedsSignIn` ("wrong user name or
    /// password" if the request that got it signed in, else naming the
    /// server's realm). Credentials for a torrent or a media link are
    /// refused.
    pub async fn add(&self, mut req: AddRequest) -> Result<JobId, CoreError> {
        self.check_running()?;
        // Resolving may take minutes: the machine must not sleep meanwhile.
        let _hold = self.hold_when_done();
        let is_dir = tokio::fs::metadata(&req.dest_dir)
            .await
            .is_ok_and(|m| m.is_dir());
        if !is_dir {
            return Err(CoreError {
                kind: ErrorKind::FolderUnavailable,
                detail: format!("folder {} is not available", req.dest_dir.display()),
            });
        }
        // One spelling per folder, so names reserved by jobs that have no
        // partial file yet are found whichever way the folder was written.
        req.dest_dir = dunce::canonicalize(&req.dest_dir).map_err(|e| CoreError {
            kind: ErrorKind::FolderUnavailable,
            detail: format!("folder {} is not available: {e}", req.dest_dir.display()),
        })?;
        // Each kind of add is boxed, so this future stays small for callers
        // on a small stack (a front end's main thread: 1 MiB on Windows).
        match classify(&req.url) {
            Ok(Source::Http(_)) => {}
            Ok(source) => {
                web_only(&req)?;
                return Box::pin(self.add_torrent(req, source)).await;
            }
            // A bad magnet link; anything else is for `resolve` to report.
            Err(e) if e.kind == ErrorKind::SiteNotSupported => return Err(e),
            Err(_) => {}
        }
        if req.only_files.is_some() {
            return Err(CoreError {
                kind: ErrorKind::Unknown,
                detail: "file selection applies only to torrents".to_string(),
            });
        }
        if req
            .checksum
            .as_deref()
            .is_some_and(|c| checksum::parse_sum(c).is_none())
        {
            return Err(CoreError {
                kind: ErrorKind::Unknown,
                detail: "invalid checksum".to_string(),
            });
        }
        let referer = req.referer.clone();
        if fast_path_host(&req.url) || req.media_info.is_some() {
            web_only(&req)?;
            return Box::pin(self.add_media(req)).await;
        }
        let url = fixed_url(&req.url);
        let auth = req.credentials.clone().and_then(|c| Auth::new(c, &url));
        let client = self.client.get();
        let resolved = resolve_with(&client, &url, referer.as_deref(), auth.as_ref());
        let probe = match Box::pin(resolved).await {
            Ok(probe) => probe,
            // A web page; yt-dlp may read it, when asked for. It never
            // gets credentials, so they are refused as for any media link.
            Err(page) if page.kind == ErrorKind::SiteNotSupported && req.media.is_some() => {
                web_only(&req)?;
                return Box::pin(self.add_media(req)).await;
            }
            Err(e) => return Err(e),
        };
        if let Some(size) = probe.size {
            check_space(
                &self.views,
                self.space,
                &req.dest_dir,
                size,
                Some(size),
                None,
            )?;
        }
        req.mirrors = Box::pin(vet_mirrors(
            &self.client.get(),
            &probe,
            &req.mirrors,
            referer.as_deref(),
            auth.as_ref(),
        ))
        .await;
        let signs_in = req.credentials.is_some();
        let added = self
            .call(|reply| Command::Add { req, probe, reply })
            .await?;
        if signs_in && added.is_ok() {
            // The job can already read its password; the store has it too
            // once `add` returns.
            self.secrets.flush().await;
        }
        added
    }

    /// Stops the job and returns once its progress is saved and its state is
    /// Paused. A Queued or Waiting job (whose retry is cancelled) becomes
    /// Paused too; pausing any other job does nothing.
    pub async fn pause(&self, id: JobId) -> Result<(), CoreError> {
        self.call(|reply| Command::Pause { id, reply }).await?
    }

    /// Re-resolves the job's link as it was added (after link fixups, with
    /// its Referer) and continues it from the link that serves the file now
    /// (a confirm form may hand out a fresh one). If the file changed on the
    /// server (validators or size) or the download cannot be
    /// resumed, it restarts from zero; a change is noted as the job's error
    /// (`FileChangedOnServer`) while it downloads. The job is queued, like a
    /// new one. Resuming a Queued, Waiting, running or completed job does
    /// nothing. A failed probe is returned and leaves the job as it was. If
    /// the rest of the file does not fit on its disk, the job is recorded as
    /// Failed with that error, which is also returned. A Paused or Failed
    /// torrent is simply queued again (its files are checked as it starts).
    pub async fn resume(&self, id: JobId) -> Result<(), CoreError> {
        self.check_running()?;
        let _hold = self.hold_when_done();
        let job = self.call(|reply| Command::Get { id, reply }).await??;
        if !matches!(job.state, JobState::Paused | JobState::Failed) {
            return Ok(());
        }
        // Mirrors are not checked again: one that fails is dropped while
        // the job runs.
        let probe = match job.kind {
            JobKind::Torrent | JobKind::Media | JobKind::Convert => None,
            JobKind::Http => {
                let auth = self.secrets.auth_for(&job).await;
                let url = fixed_url(&job.source_url);
                let referer = job.referer.as_deref();
                let client = self.client.get();
                Some(resolve_with(&client, &url, referer, auth.as_ref()).await?)
            }
        };
        self.call(|reply| Command::Resume { id, probe, reply })
            .await?
    }

    /// Starts a Queued or Waiting job at once, even when `max_active` jobs
    /// already download; it counts toward them while it runs. Any other job
    /// is left as it is. If the rest of the file does not fit on its disk
    /// beside what the running jobs there still download, the job is
    /// recorded as Failed with that error, which is also returned.
    pub async fn start_now(&self, id: JobId) -> Result<(), CoreError> {
        let _hold = self.hold_when_done();
        self.call(|reply| Command::StartNow { id, reply }).await?
    }

    /// Stops the job if it runs and forgets it. With `delete_files`, also
    /// deletes its partial file, or the downloaded file of a completed job,
    /// unless another job uses the same name or the downloaded file's size
    /// has changed. A torrent leaves the torrent session; with
    /// `delete_files` its own files go (those whose size is still the
    /// torrent's), then the folders they leave empty, and the folder it was
    /// given if that is empty. A media job with `delete_files` loses its
    /// finished files and the partial files its runs recorded writing (as
    /// yt-dlp reported them, never found by name, so never another job's),
    /// but not a playlist's folder. With `RemoveFiles::IfUnfinished` (see
    /// `remove_with`) an unfinished media job loses only those partial
    /// files: what it finished (a paused playlist's done videos) is kept.
    /// A job's password (see `set_credentials`) is deleted in every case.
    pub async fn remove(&self, id: JobId, delete_files: bool) -> Result<(), CoreError> {
        let files = if delete_files {
            RemoveFiles::All
        } else {
            RemoveFiles::Keep
        };
        self.remove_with(id, files).await
    }

    /// Like `remove`, with the choice of files made by the engine: with
    /// `RemoveFiles::IfUnfinished` it looks at the job as it is when it
    /// removes it, so a download that finished a moment ago keeps its file.
    pub async fn remove_with(&self, id: JobId, files: RemoveFiles) -> Result<(), CoreError> {
        let removed = self
            .call(|reply| Command::Remove { id, files, reply })
            .await?;
        // The job's password is deleted from the store before this returns.
        self.secrets.flush().await;
        removed
    }

    /// Every job, ordered by id.
    pub fn snapshot(&self) -> Vec<JobView> {
        let mut views: Vec<JobView> = read(&self.views).values().cloned().collect();
        views.sort_by_key(|v| v.id);
        views
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    /// The current settings.
    pub fn settings(&self) -> Settings {
        read(&self.settings).clone()
    }

    /// Saves the settings and applies them at once, also to running jobs,
    /// then emits `SettingsChanged`. A higher `max_active` starts more
    /// Queued jobs; a lower one lets running jobs finish (likewise
    /// `torrent.max_active` for torrents). A `max_active` or
    /// `torrent.max_active` of 0 is refused, and so is a manual proxy that
    /// `network::parse_proxy` refuses. A new proxy applies to every web
    /// request from the next one on and to yt-dlp's next run (a running
    /// video download keeps its own) and to the torrent session's next
    /// start: a running session keeps its proxy until the engine opens again
    /// (`network::torrent_proxy_changes` tells whether the change matters to
    /// torrents). The torrent port, DHT and UPnP apply the next time the
    /// engine opens. Nothing changes if saving fails.
    pub async fn set_settings(&self, settings: Settings) -> Result<(), CoreError> {
        self.call(|reply| Command::SetSettings { settings, reply })
            .await?
    }

    /// Stops the when-done countdown, if one runs, and emits
    /// `WhenDoneCancelled`. Adding, resuming or starting a job (from the
    /// moment it is called), a `WhenDoneHold`, or changing `when_done`
    /// stops it too.
    pub async fn cancel_when_done(&self) -> Result<(), CoreError> {
        self.call(|reply| Command::CancelWhenDone { reply }).await?
    }

    /// Holds off the when-done action until the returned guard (and every
    /// other hold) is dropped: taking it stops a running countdown, and
    /// while held no countdown starts and no action fires. A queue that went
    /// idle meanwhile because a job finished runs the action once the last
    /// hold is dropped; a stopped countdown does not start again. For front
    /// ends that add several jobs in a row; `add`, `resume` and `start_now`
    /// each hold it while they run.
    pub fn hold_when_done(&self) -> WhenDoneHold {
        WhenDoneHold::new(&self.holds, &self.hold_tx)
    }

    /// Whether the engine asks the OS to keep the computer awake: while a
    /// job downloads and `keep_awake` is on. The OS may not comply.
    pub fn keeping_awake(&self) -> bool {
        self.keeping_awake.load(Ordering::Relaxed)
    }

    /// Resolves once the job is Completed, Paused or Failed (at once if it
    /// already is); a Queued, Waiting or Seeding job is waited for until it
    /// has run.
    /// An unknown job resolves to Failed immediately. If the engine stops
    /// first, resolves to the job's last state.
    pub async fn wait(&self, id: JobId) -> JobState {
        match self.call(|reply| Command::Wait { id, reply }).await {
            Ok(state) => state,
            Err(_) => self.view(id).map_or(JobState::Failed, |v| v.state),
        }
    }

    /// Stops every running job (saving its progress) and records it as
    /// Queued, not Paused, so the next open starts it again (checked against
    /// the server first, as after a crash); closes the
    /// database and stops the engine. Later calls on any clone fail with
    /// "engine stopped"; calling `shutdown` again does nothing.
    pub async fn shutdown(&self) -> Result<(), CoreError> {
        // An error here means the engine has already stopped.
        let _ = self.call(|reply| Command::Shutdown { reply }).await;
        // Passwords being written reach the store (unless it hangs); none
        // stays in memory.
        self.secrets.flush().await;
        self.secrets.clear_memory();
        Ok(())
    }

    fn view(&self, id: JobId) -> Option<JobView> {
        read(&self.views).get(&id).cloned()
    }

    /// Fails fast before network work when the engine has stopped.
    fn check_running(&self) -> Result<(), CoreError> {
        if self.commands.is_closed() {
            Err(stopped())
        } else {
            Ok(())
        }
    }

    async fn call<T>(
        &self,
        command: impl FnOnce(oneshot::Sender<T>) -> Command,
    ) -> Result<T, CoreError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(command(reply))
            .await
            .map_err(|_| stopped())?;
        response.await.map_err(|_| stopped())
    }
}

/// Holds off the when-done action while alive; see `Engine::hold_when_done`.
pub struct WhenDoneHold {
    holds: Arc<AtomicUsize>,
    tx: mpsc::UnboundedSender<JobMsg>,
}

impl WhenDoneHold {
    fn new(holds: &Arc<AtomicUsize>, tx: &mpsc::UnboundedSender<JobMsg>) -> WhenDoneHold {
        holds.fetch_add(1, Ordering::SeqCst);
        // A stopped engine has nothing to hold off.
        let _ = tx.send(JobMsg::HoldChanged);
        WhenDoneHold {
            holds: holds.clone(),
            tx: tx.clone(),
        }
    }
}

impl Drop for WhenDoneHold {
    fn drop(&mut self) {
        self.holds.fetch_sub(1, Ordering::SeqCst);
        let _ = self.tx.send(JobMsg::HoldChanged);
    }
}

fn read<T>(lock: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    // Nothing panics while holding the lock, so a poisoned value is still whole.
    lock.read().unwrap_or_else(|p| p.into_inner())
}

fn write<T>(lock: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    lock.write().unwrap_or_else(|p| p.into_inner())
}

fn io_error(e: std::io::Error) -> CoreError {
    CoreError {
        kind: classify_io(&e),
        detail: e.to_string(),
    }
}

impl CoreError {
    /// Whether this is `Engine::open` refusing a data dir that another
    /// engine (in this or another process) holds.
    pub fn is_instance_locked(&self) -> bool {
        self.kind == ErrorKind::Unknown && self.detail.starts_with(IN_USE)
    }

    /// Whether this is `read_snapshot` refusing a database of an older
    /// schema, which only `Engine::open` (with the data dir free) upgrades.
    pub fn is_older_schema(&self) -> bool {
        self.kind == ErrorKind::Unknown && self.detail.starts_with(crate::store::OLDER_SCHEMA)
    }
}

fn no_such_job() -> CoreError {
    CoreError {
        kind: ErrorKind::Unknown,
        detail: "no such job".to_string(),
    }
}

fn stopped() -> CoreError {
    CoreError {
        kind: ErrorKind::Unknown,
        detail: "engine stopped".to_string(),
    }
}
