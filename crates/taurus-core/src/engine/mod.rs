//! The public entry point: a cheap-to-clone [`Engine`] handle over one
//! background actor that owns the running jobs, restores jobs from the store
//! on open and broadcasts [`Event`]s.
//!
//! Network probes run in the caller's task; everything that touches the
//! running jobs or reserves names runs in the actor, one command at a time.

mod actor;
mod inspect;
mod jobs;
mod media;
mod media_job;
mod open;
mod remove;
mod schedule;
mod settings;
mod stop_requests;
mod tool_ops;
mod torrent;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::Duration;

use tokio::sync::{broadcast, mpsc, oneshot};

use crate::checksum;
use crate::errors::{CoreError, ErrorKind, classify_io};
use crate::http::resolve;
use crate::media::fast_path_host;
use crate::settings::{PowerMode, Settings, WhenDone};
use crate::store::{JobId, JobKind, JobState, Store};
use crate::system::{SpaceSource, available_space};
use crate::timing::Timing;
use crate::tools::{ToolManager, ToolSources, ToolStatus};
use crate::torrent::hub::{TorrentHub, TorrentTestNet};
use crate::torrent::source::{Source, classify};

use actor::Command;
#[doc(hidden)]
pub use actor::JobMsg;
pub use inspect::LinkInfo;
use jobs::{downloaded, fixed_url, vet_mirrors, view_of};
pub use media::{MediaChoice, MediaView};
use open::assemble;
pub use remove::RemoveFiles;
use schedule::check_space;

const COMMAND_CAPACITY: usize = 64;
const EVENT_CAPACITY: usize = 1024;
const DB_FILE: &str = "taurus.db";
const LOCK_FILE: &str = "taurus.lock";
/// Start of the detail of the error `Engine::open` returns when another
/// engine holds the data dir.
const IN_USE: &str = "another taurus instance is using";

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
    /// Defaults to `DEFAULT_CONNECTIONS`. Web downloads only.
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
    client: reqwest::Client,
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
        ToolManager::new(
            &config.data_dir,
            config.tool_sources.clone(),
            reqwest::Client::new(),
        )
        .status()
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
    /// detail `playlist:<count>` unless `confirm_large_playlist`; mirrors
    /// and a checksum are refused. It counts toward `max_active` like a web
    /// download; see `stop_recording` for live streams.
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
            Ok(source) => return Box::pin(self.add_torrent(req, source)).await,
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
        if fast_path_host(&req.url) {
            return Box::pin(self.add_media(req)).await;
        }
        let url = fixed_url(&req.url);
        let probe = match Box::pin(resolve(&self.client, &url, referer.as_deref())).await {
            Ok(probe) => probe,
            // A web page; yt-dlp may read it, when asked for.
            Err(page) if page.kind == ErrorKind::SiteNotSupported && req.media.is_some() => {
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
            &self.client,
            &probe,
            &req.mirrors,
            referer.as_deref(),
        ))
        .await;
        self.call(|reply| Command::Add { req, probe, reply })
            .await?
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
            JobKind::Torrent | JobKind::Media => None,
            JobKind::Http => Some(
                resolve(
                    &self.client,
                    &fixed_url(&job.source_url),
                    job.referer.as_deref(),
                )
                .await?,
            ),
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
    /// given if that is empty.
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
        self.call(|reply| Command::Remove { id, files, reply })
            .await?
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
    /// `torrent.max_active` of 0 is refused. The torrent port, DHT and UPnP
    /// apply the next time the engine opens. Nothing changes if saving
    /// fails.
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
