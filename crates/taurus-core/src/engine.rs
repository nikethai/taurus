//! The public entry point: a cheap-to-clone [`Engine`] handle over one
//! background actor that owns the running jobs, restores jobs from the store
//! on open and broadcasts [`Event`]s.
//!
//! Network probes run in the caller's task; everything that touches the
//! running jobs or reserves names runs in the actor, one command at a time.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::task::AbortHandle;
use tokio_util::sync::CancellationToken;

use crate::checksum;
use crate::errors::{CoreError, ErrorKind, classify_io};
use crate::filename::{PARTIAL_EXT, sanitize, unique_path};
use crate::fixups;
use crate::http::probe::validator_change;
use crate::http::{JobContext, JobOutcome, Probe, build_client, resolve, run_job};
use crate::limit::{Limiter, PeakTracker};
use crate::plan::DEFAULT_CONNECTIONS;
use crate::settings::{PowerMode, Settings, SpeedLimit, WhenDone};
use crate::store::{JobId, JobRecord, JobState, NewJob, Store};
use crate::system::marks::loggable;
use crate::system::{AwakeGuard, check_fits};
use crate::timing::Timing;
use crate::when_done::WhenDoneRunner;

const COMMAND_CAPACITY: usize = 64;
const EVENT_CAPACITY: usize = 1024;
const DB_FILE: &str = "taurus.db";
const LOCK_FILE: &str = "taurus.lock";
/// Start of the detail of the error `Engine::open` returns when another
/// engine holds the data dir.
const IN_USE: &str = "another taurus instance is using";
/// A measured peak older than this is replaced by the next measurement, even
/// a lower one.
const PEAK_MAX_AGE_SECS: i64 = 7 * 24 * 60 * 60;

pub struct EngineConfig {
    /// Holds the database, `taurus.db`, and the lock file, `taurus.lock`.
    /// Created if missing.
    pub data_dir: PathBuf,
    /// Retry and countdown delays.
    pub timing: Timing,
    /// Whether when-done actions that touch the OS really run.
    pub power: PowerMode,
    /// Whether the queue runs on its own. Without it, no job starts, is
    /// retried or is restored to the queue unless `start_now` starts it:
    /// for short-lived front ends that only look at or edit the jobs.
    pub autostart: bool,
}

impl EngineConfig {
    /// Default timing; when-done actions run for real.
    pub fn new(data_dir: impl Into<PathBuf>) -> EngineConfig {
        EngineConfig {
            data_dir: data_dir.into(),
            timing: Timing::default(),
            power: PowerMode::Execute,
            autostart: true,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct AddRequest {
    pub url: String,
    pub dest_dir: PathBuf,
    /// Overrides the name the server suggests; sanitized before use.
    pub file_name: Option<String>,
    /// Defaults to `DEFAULT_CONNECTIONS`.
    pub connections: Option<u32>,
    /// Other URLs serving the same file. Each is checked at `add`; one that
    /// fails to resolve, serves another size or another version (by its
    /// validators) is dropped.
    pub mirrors: Vec<String>,
    /// The file's expected checksum, stored with the job.
    pub checksum: Option<String>,
    /// Sent as `Referer` with every request of the job.
    pub referer: Option<String>,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct JobView {
    pub id: JobId,
    pub url: String,
    pub file_name: String,
    pub dest_dir: PathBuf,
    pub state: JobState,
    pub total: Option<u64>,
    pub downloaded: u64,
    /// 0 unless the job is running.
    pub speed_bps: u64,
    pub resumable: bool,
    pub error: Option<(ErrorKind, String)>,
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
    /// time the last such job leaves those states.
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
    /// command line that ran (or would have, in `PowerMode::DryRun`).
    WhenDoneFired {
        action: WhenDone,
        command: Option<String>,
    },
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
}

type Reply<T> = oneshot::Sender<Result<T, CoreError>>;

enum Command {
    Add {
        req: AddRequest,
        probe: Probe,
        reply: Reply<JobId>,
    },
    Pause {
        id: JobId,
        reply: Reply<()>,
    },
    Resume {
        id: JobId,
        probe: Probe,
        reply: Reply<()>,
    },
    StartNow {
        id: JobId,
        reply: Reply<()>,
    },
    Get {
        id: JobId,
        reply: Reply<JobRecord>,
    },
    Remove {
        id: JobId,
        delete_files: bool,
        reply: Reply<()>,
    },
    Wait {
        id: JobId,
        reply: oneshot::Sender<JobState>,
    },
    SetSettings {
        settings: Settings,
        reply: Reply<()>,
    },
    CancelWhenDone {
        reply: Reply<()>,
    },
    Shutdown {
        reply: oneshot::Sender<()>,
    },
}

/// Sent by a job's task (all of a run's progress precedes its `Finished`),
/// or by a Waiting job's retry timer.
enum JobMsg {
    Progress {
        id: JobId,
        downloaded: u64,
        total: Option<u64>,
        speed_bps: u64,
    },
    Finished {
        id: JobId,
        outcome: JobOutcome,
    },
    /// Timer `seq` of the job ran out.
    RetryDue {
        id: JobId,
        seq: u64,
    },
}

impl Engine {
    /// Opens the database in `config.data_dir`, restores its jobs, applies
    /// the saved settings (as much of them as reads; see
    /// `Store::load_settings_lenient`) and starts the queue. Jobs recorded as Downloading
    /// (interrupted) become Paused; Waiting jobs are queued again at once,
    /// with Queued ones. Without `config.autostart`, Queued and Waiting jobs
    /// are left as stored.
    ///
    /// The engine holds `data_dir` exclusively until it shuts down or its
    /// last handle is dropped. While another engine (in any process) holds
    /// it, `open` fails without writing anything and the error's
    /// `is_instance_locked` is true; `read_snapshot` still works.
    pub async fn open(config: EngineConfig) -> Result<Engine, CoreError> {
        tokio::fs::create_dir_all(&config.data_dir)
            .await
            .map_err(io_error)?;
        let lock = lock_data_dir(&config.data_dir)?;
        let store = Store::open(&config.data_dir.join(DB_FILE))?;
        // Unreadable settings must not keep the user out, not even from
        // fixing them.
        let settings = store.load_settings_lenient()?;
        // Only a measurement: an unreadable one is measured again.
        let measured_peak = store.measured_peak().unwrap_or_else(|e| {
            tracing::warn!("ignoring the saved peak speed: {e}");
            None
        });
        let mut views = HashMap::new();
        for mut job in store.list_jobs()? {
            match job.state {
                // Nothing is running yet.
                JobState::Downloading => {
                    job.state = JobState::Paused;
                    store.update_job(&job)?;
                }
                // The connection may well be back by now.
                JobState::Waiting if config.autostart => {
                    store.requeue(job.id)?;
                    job.state = JobState::Queued;
                    job.error = None;
                }
                _ => {}
            }
            let downloaded = downloaded(&store, job.id)?;
            views.insert(job.id, view_of(&job, downloaded));
        }

        let views = Arc::new(RwLock::new(views));
        let shared_settings = Arc::new(RwLock::new(settings.clone()));
        let keeping_awake = Arc::new(AtomicBool::new(false));
        let limiter = Limiter::new(speed_limit(&settings, measured_peak));
        let peak = PeakTracker::with_max(fresh_peak(measured_peak, unix_now()).unwrap_or(0));
        let (commands, commands_rx) = mpsc::channel(COMMAND_CAPACITY);
        let (events, _) = broadcast::channel(EVENT_CAPACITY);
        let (job_tx, job_rx) = mpsc::unbounded_channel();
        let client = build_client();
        let mut actor = Actor {
            _lock: lock,
            store,
            client: client.clone(),
            views: views.clone(),
            events: events.clone(),
            running: HashMap::new(),
            waiters: HashMap::new(),
            retries: HashMap::new(),
            retry_counts: HashMap::new(),
            next_retry_seq: 0,
            busy: false,
            stopping: false,
            autostart: config.autostart,
            when_done: WhenDoneRunner::new(
                events.clone(),
                config.power,
                config.timing.when_done_countdown,
            ),
            awake: AwakeGuard::new(),
            keeping_awake: keeping_awake.clone(),
            job_tx,
            job_rx,
            timing: config.timing.clone(),
            settings,
            shared_settings: shared_settings.clone(),
            limiter,
            measured_peak,
            peak,
            transferred: 0,
        };
        actor.after_change();
        tokio::spawn(actor.run(commands_rx));
        Ok(Engine {
            commands,
            events,
            views,
            settings: shared_settings,
            keeping_awake,
            client,
        })
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
            .map(|job| Ok(view_of(job, downloaded(&store, job.id)?)))
            .collect()
    }

    /// Resolves the URL (following a confirm page; failing on any other web
    /// page or bot check), keeps the mirrors that serve the same file,
    /// reserves a file name that no other file or job in `dest_dir` uses
    /// (ignoring case), and queues the job; it starts once fewer than
    /// `max_active` jobs download. The job records `dest_dir` canonicalized
    /// (symlinks and `.` resolved). A missing `dest_dir` fails with
    /// `FolderUnavailable`, a file that does not fit on its disk with
    /// `DiskFull` or `FileTooLargeForDisk`, a checksum that is not a SHA-256
    /// with `Unknown`; nothing is recorded when `add` fails.
    pub async fn add(&self, mut req: AddRequest) -> Result<JobId, CoreError> {
        self.check_running()?;
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
        let probe = resolve(&self.client, &fixed_url(&req.url), referer.as_deref()).await?;
        if let Some(size) = probe.size {
            check_fits(&req.dest_dir, size, Some(size))?;
        }
        req.mirrors = vet_mirrors(&self.client, &probe, &req.mirrors, referer.as_deref()).await;
        self.call(|reply| Command::Add { req, probe, reply })
            .await?
    }

    /// Stops the job and returns once its progress is saved and its state is
    /// Paused. A Queued or Waiting job (whose retry is cancelled) becomes
    /// Paused too; pausing any other job does nothing.
    pub async fn pause(&self, id: JobId) -> Result<(), CoreError> {
        self.call(|reply| Command::Pause { id, reply }).await?
    }

    /// Re-resolves the job's URL (with its Referer) and continues it from
    /// the link that serves the file now (a confirm form may hand out a fresh
    /// one). If the file changed on the
    /// server (validators or size) or the download cannot be
    /// resumed, it restarts from zero; a change is noted as the job's error
    /// (`FileChangedOnServer`) while it downloads. The job is queued, like a
    /// new one. Resuming a Queued, Waiting, running or completed job does
    /// nothing. A failed probe is returned and leaves the job as it was. If
    /// the rest of the file does not fit on its disk, the job is recorded as
    /// Failed with that error, which is also returned.
    pub async fn resume(&self, id: JobId) -> Result<(), CoreError> {
        self.check_running()?;
        let job = self.call(|reply| Command::Get { id, reply }).await??;
        if !matches!(job.state, JobState::Paused | JobState::Failed) {
            return Ok(());
        }
        // Mirrors are not checked again: one that fails is dropped while
        // the job runs.
        let probe = resolve(&self.client, &job.url, job.referer.as_deref()).await?;
        self.call(|reply| Command::Resume { id, probe, reply })
            .await?
    }

    /// Starts a Queued or Waiting job at once, even when `max_active` jobs
    /// already download; it counts toward them while it runs. Any other job
    /// is left as it is.
    pub async fn start_now(&self, id: JobId) -> Result<(), CoreError> {
        self.call(|reply| Command::StartNow { id, reply }).await?
    }

    /// Stops the job if it runs and forgets it. With `delete_files`, also
    /// deletes its partial file, or the downloaded file of a completed job,
    /// unless another job uses the same name or the downloaded file's size
    /// has changed.
    pub async fn remove(&self, id: JobId, delete_files: bool) -> Result<(), CoreError> {
        self.call(|reply| Command::Remove {
            id,
            delete_files,
            reply,
        })
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
    /// Queued jobs; a lower one lets running jobs finish. A `max_active` of
    /// 0 is refused. Nothing changes if saving fails.
    pub async fn set_settings(&self, settings: Settings) -> Result<(), CoreError> {
        self.call(|reply| Command::SetSettings { settings, reply })
            .await?
    }

    /// Stops the when-done countdown, if one runs, and emits
    /// `WhenDoneCancelled`. Adding, resuming or starting a job, or changing
    /// `when_done`, stops it too.
    pub async fn cancel_when_done(&self) -> Result<(), CoreError> {
        self.call(|reply| Command::CancelWhenDone { reply }).await?
    }

    /// Whether the engine asks the OS to keep the computer awake: while a
    /// job downloads and `keep_awake` is on. The OS may not comply.
    pub fn keeping_awake(&self) -> bool {
        self.keeping_awake.load(Ordering::Relaxed)
    }

    /// Resolves once the job is Completed, Paused or Failed (at once if it
    /// already is); a Queued or Waiting job is waited for until it has run.
    /// An unknown job resolves to Failed immediately. If the engine stops
    /// first, resolves to the job's last state.
    pub async fn wait(&self, id: JobId) -> JobState {
        match self.call(|reply| Command::Wait { id, reply }).await {
            Ok(state) => state,
            Err(_) => self.view(id).map_or(JobState::Failed, |v| v.state),
        }
    }

    /// Pauses every running job (saving its progress), closes the database
    /// and stops the engine. Later calls on any clone fail with "engine
    /// stopped"; calling `shutdown` again does nothing.
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

struct Actor {
    /// Holds the data dir; released when the actor stops.
    _lock: std::fs::File,
    store: Store,
    client: reqwest::Client,
    views: Views,
    events: broadcast::Sender<Event>,
    /// The jobs whose download task is running.
    running: HashMap<JobId, Run>,
    /// `wait` callers of jobs that are not Completed, Paused or Failed yet.
    waiters: HashMap<JobId, Vec<oneshot::Sender<JobState>>>,
    /// Retry timers of Waiting jobs.
    retries: HashMap<JobId, Retry>,
    /// Waits in a row without progress, per job: the next wait doubles.
    retry_counts: HashMap<JobId, u32>,
    next_retry_seq: u64,
    /// Whether a job was Downloading, Queued or Waiting at the last change.
    busy: bool,
    /// Shutting down: start nothing more.
    stopping: bool,
    /// Whether jobs start and retry on their own; else only `start_now`.
    autostart: bool,
    when_done: WhenDoneRunner,
    awake: AwakeGuard,
    keeping_awake: Arc<AtomicBool>,
    job_tx: mpsc::UnboundedSender<JobMsg>,
    job_rx: mpsc::UnboundedReceiver<JobMsg>,
    timing: Timing,
    settings: Settings,
    shared_settings: Arc<RwLock<Settings>>,
    /// Shared by every job's workers.
    limiter: Limiter,
    /// The saved peak speed (bytes/s) and when it was measured (unix secs).
    measured_peak: Option<(u64, i64)>,
    peak: PeakTracker,
    /// Bytes downloaded by all jobs since the engine opened.
    transferred: u64,
}

struct Run {
    cancel: CancellationToken,
    /// Durable progress when the run started.
    downloaded_at_start: u64,
}

struct Retry {
    /// Tells this timer's `RetryDue` from an older, cancelled one's.
    seq: u64,
    timer: AbortHandle,
}

impl Actor {
    /// Serves commands until `shutdown` or until every handle is dropped.
    async fn run(mut self, mut commands: mpsc::Receiver<Command>) {
        let shutdown = loop {
            tokio::select! {
                command = commands.recv() => match command {
                    Some(command) => {
                        if let Some(reply) = self.handle(command).await {
                            break Some(reply);
                        }
                    }
                    None => break None,
                },
                Some(msg) = self.job_rx.recv() => self.on_job_msg(msg),
            }
        };
        // New calls fail from here on; queued ones are dropped with `commands`.
        commands.close();
        self.when_done.cancel();
        // Queued and Waiting jobs stay so, for the next open to start.
        self.stopping = true;
        for (_, retry) in self.retries.drain() {
            retry.timer.abort();
        }
        self.pause_all().await;
        // Closes the database before anyone hears that the engine stopped.
        drop(self);
        drop(commands);
        if let Some(reply) = shutdown {
            let _ = reply.send(());
        }
    }

    /// Runs one command; returns the reply of a `Shutdown`.
    async fn handle(&mut self, command: Command) -> Option<oneshot::Sender<()>> {
        // New work stops the when-done countdown.
        if matches!(
            command,
            Command::Add { .. } | Command::Resume { .. } | Command::StartNow { .. }
        ) {
            self.when_done.cancel();
        }
        match command {
            Command::Add { req, probe, reply } => {
                let _ = reply.send(self.add(req, &probe));
            }
            Command::Pause { id, reply } => {
                let _ = reply.send(self.pause(id).await);
            }
            Command::Resume { id, probe, reply } => {
                let _ = reply.send(self.resume(id, &probe));
            }
            Command::StartNow { id, reply } => {
                let _ = reply.send(self.start_now(id));
            }
            Command::Get { id, reply } => {
                let _ = reply.send(
                    self.store
                        .get_job(id)
                        .and_then(|j| j.ok_or_else(no_such_job)),
                );
            }
            Command::Remove {
                id,
                delete_files,
                reply,
            } => {
                let _ = reply.send(self.remove(id, delete_files).await);
            }
            Command::Wait { id, reply } => self.wait(id, reply),
            Command::SetSettings { settings, reply } => {
                let _ = reply.send(self.set_settings(settings));
            }
            Command::CancelWhenDone { reply } => {
                self.when_done.cancel();
                let _ = reply.send(Ok(()));
            }
            Command::Shutdown { reply } => return Some(reply),
        }
        None
    }

    fn add(&mut self, req: AddRequest, probe: &Probe) -> Result<JobId, CoreError> {
        let name = match &req.file_name {
            Some(name) => sanitize(name),
            None => probe.file_name.clone(),
        };
        // Reserving here, in the single actor, keeps concurrent adds apart.
        let reserved = self.store.reserved_names(&req.dest_dir)?;
        let path = unique_path(&req.dest_dir, &name, &reserved);
        let file_name = path
            .file_name()
            .map_or(name, |n| n.to_string_lossy().into_owned());
        let connections = req.connections.unwrap_or(DEFAULT_CONNECTIONS).max(1);
        // The link that served the file (a confirm form's, if one was
        // followed), so segment requests skip the page.
        let url = probe.requested_url.to_string();
        let id = self.store.insert_job(&NewJob {
            url: url.clone(),
            dest_dir: req.dest_dir.clone(),
            file_name: file_name.clone(),
            connections,
            mirrors: req.mirrors.clone(),
            checksum: req.checksum.clone(),
            referer: req.referer.clone(),
            queue_pos: 0,
        })?;
        let mut job = JobRecord {
            id,
            url,
            final_url: None,
            dest_dir: req.dest_dir,
            file_name,
            state: JobState::Queued,
            total: None,
            resumable: false,
            etag: None,
            last_modified: None,
            connections,
            error: None,
            mirrors: req.mirrors,
            checksum: req.checksum,
            referer: req.referer,
            queue_pos: id.0,
        };
        apply_probe(&mut job, probe);
        if let Err(e) = self.store.update_job(&job) {
            if let Err(cleanup) = self.store.delete_job(id) {
                tracing::warn!(job = id.0, "could not remove a half-added job: {cleanup}");
            }
            return Err(e);
        }
        let view = view_of(&job, 0);
        write(&self.views).insert(id, view.clone());
        self.emit(Event::JobAdded(view));
        self.after_change();
        Ok(id)
    }

    async fn pause(&mut self, id: JobId) -> Result<(), CoreError> {
        if !read(&self.views).contains_key(&id) {
            return Err(no_such_job());
        }
        if let Some(run) = self.running.get(&id) {
            run.cancel.cancel();
            self.settle(&[id]).await;
        }
        // Not running, or it failed (and now waits) as it stopped.
        let result = if matches!(
            self.state_of(id),
            Some(JobState::Queued | JobState::Waiting)
        ) {
            self.cancel_retry(id);
            self.store
                .set_state(id, JobState::Paused)
                .map(|()| self.set_view_state(id, JobState::Paused))
        } else {
            Ok(())
        };
        self.retry_counts.remove(&id);
        self.after_change();
        result
    }

    fn resume(&mut self, id: JobId, probe: &Probe) -> Result<(), CoreError> {
        let mut job = self.store.get_job(id)?.ok_or_else(no_such_job)?;
        if self.running.contains_key(&id)
            || !matches!(job.state, JobState::Paused | JobState::Failed)
        {
            return Ok(());
        }
        // The link that served the file now: a confirm form may hand out a
        // fresh one.
        job.url = probe.requested_url.to_string();
        let change = change_on_server(&job, probe);
        let resumable = probe.ranges && probe.size.is_some();
        if change.is_some() || !job.resumable || !resumable {
            // Never stitch two versions of a file: start over.
            remove_if_exists(&partial_path(&job))?;
            self.store.replace_segments(id, &[])?;
            apply_probe(&mut job, probe);
        }
        if let Some(what) = &change {
            tracing::info!(job = id.0, "restarting from zero: {what}");
        }
        job.error = change.map(|what| (ErrorKind::FileChangedOnServer, what));
        job.state = JobState::Queued;
        let done = downloaded(&self.store, id)?;
        let fits = match job.total {
            Some(total) => check_fits(&job.dest_dir, total.saturating_sub(done), Some(total)),
            None => Ok(()),
        };
        if let Err(e) = &fits {
            job.state = JobState::Failed;
            job.error = Some((e.kind, e.detail.clone()));
        }
        self.store.update_job(&job)?;
        let view = view_of(&job, done);
        write(&self.views).insert(id, view.clone());
        self.emit(Event::JobUpdated(view));
        fits?;
        self.retry_counts.remove(&id);
        self.after_change();
        Ok(())
    }

    fn start_now(&mut self, id: JobId) -> Result<(), CoreError> {
        let state = self.state_of(id).ok_or_else(no_such_job)?;
        if self.running.contains_key(&id) {
            return Ok(());
        }
        match state {
            JobState::Queued => {}
            JobState::Waiting => {
                self.cancel_retry(id);
                self.requeue(id)?;
            }
            _ => return Ok(()),
        }
        self.start(id);
        self.after_change();
        Ok(())
    }

    async fn remove(&mut self, id: JobId, delete_files: bool) -> Result<(), CoreError> {
        if !read(&self.views).contains_key(&id) {
            return Err(no_such_job());
        }
        if let Some(run) = self.running.get(&id) {
            run.cancel.cancel();
            self.settle(&[id]).await;
        }
        self.cancel_retry(id);
        self.retry_counts.remove(&id);
        let removed = self.delete(id, delete_files);
        self.after_change();
        removed
    }

    fn delete(&mut self, id: JobId, delete_files: bool) -> Result<(), CoreError> {
        if delete_files && let Some(job) = self.store.get_job(id)? {
            delete_job_files(&self.store, &job)?;
        }
        self.store.delete_job(id)?;
        write(&self.views).remove(&id);
        self.emit(Event::JobRemoved(id));
        Ok(())
    }

    fn wait(&mut self, id: JobId, reply: oneshot::Sender<JobState>) {
        match self.state_of(id) {
            Some(state) if !is_final(state) => self.waiters.entry(id).or_default().push(reply),
            state => {
                let _ = reply.send(state.unwrap_or(JobState::Failed));
            }
        }
    }

    fn set_settings(&mut self, settings: Settings) -> Result<(), CoreError> {
        // Queued jobs, and their waiters, would wait for ever.
        if settings.max_active == 0 {
            return Err(CoreError {
                kind: ErrorKind::Unknown,
                detail: "max_active must be at least 1".to_string(),
            });
        }
        self.store.save_settings(&settings)?;
        if settings.when_done != self.settings.when_done {
            self.when_done.cancel();
        }
        self.limiter
            .set_rate(speed_limit(&settings, self.measured_peak));
        *write(&self.shared_settings) = settings.clone();
        self.settings = settings.clone();
        self.emit(Event::SettingsChanged(settings));
        self.after_change();
        Ok(())
    }

    async fn pause_all(&mut self) {
        let ids: Vec<JobId> = self.running.keys().copied().collect();
        for run in self.running.values() {
            run.cancel.cancel();
        }
        self.settle(&ids).await;
    }

    /// Runs after every change of a job's state or of the settings: starts
    /// Queued jobs in queue order while fewer than `max_active` download,
    /// announces `QueueIdle` when the last busy job leaves and runs the
    /// when-done action (only with `autostart`), keeps the computer awake
    /// while a job downloads, then answers the waiters of jobs that are done
    /// (after their events).
    fn after_change(&mut self) {
        if !self.stopping {
            self.schedule();
            let busy = read(&self.views).values().any(|v| {
                matches!(
                    v.state,
                    JobState::Downloading | JobState::Queued | JobState::Waiting
                )
            });
            if self.busy && !busy {
                self.emit(Event::QueueIdle);
                if self.autostart {
                    self.when_done.queue_idle(self.settings.when_done);
                }
            }
            self.busy = busy;
        }
        let downloading = read(&self.views)
            .values()
            .any(|v| v.state == JobState::Downloading);
        self.awake.set(self.settings.keep_awake && downloading);
        self.keeping_awake
            .store(self.awake.is_requested(), Ordering::Relaxed);
        let views = read(&self.views);
        self.waiters.retain(|id, waiters| {
            let state = match views.get(id) {
                Some(view) if !is_final(view.state) => return true,
                Some(view) => view.state,
                // Removed.
                None => JobState::Failed,
            };
            for waiter in waiters.drain(..) {
                let _ = waiter.send(state);
            }
            false
        });
    }

    fn schedule(&mut self) {
        if !self.autostart {
            return;
        }
        let max_active = usize::try_from(self.settings.max_active).unwrap_or(usize::MAX);
        if self.running.len() >= max_active {
            return;
        }
        let queued = match self.store.queued_jobs() {
            Ok(queued) => queued,
            Err(e) => {
                tracing::warn!("could not read the queue: {e}");
                return;
            }
        };
        for id in queued {
            if self.running.len() >= max_active {
                break;
            }
            if !self.running.contains_key(&id) && self.state_of(id) == Some(JobState::Queued) {
                self.start(id);
            }
        }
    }

    /// Retries the Waiting job after `waiting_retry_first`, doubling per wait
    /// in a row in which it made no progress, up to `waiting_retry_cap`.
    fn retry_later(&mut self, id: JobId, progressed: bool) {
        let count = self.retry_counts.entry(id).or_insert(0);
        if progressed {
            *count = 0;
        }
        let delay = retry_delay(&self.timing, *count);
        *count = count.saturating_add(1);
        self.next_retry_seq += 1;
        let seq = self.next_retry_seq;
        let tx = self.job_tx.clone();
        let timer = tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            let _ = tx.send(JobMsg::RetryDue { id, seq });
        })
        .abort_handle();
        if let Some(old) = self.retries.insert(id, Retry { seq, timer }) {
            old.timer.abort();
        }
    }

    fn on_retry_due(&mut self, id: JobId, seq: u64) {
        // A cancelled timer may have fired before it was aborted.
        if self.retries.get(&id).is_none_or(|r| r.seq != seq) {
            return;
        }
        self.retries.remove(&id);
        if self.state_of(id) == Some(JobState::Waiting) {
            if let Err(e) = self.requeue(id) {
                tracing::warn!(job = id.0, "could not queue the waiting job: {e}");
            }
            self.after_change();
        }
    }

    fn cancel_retry(&mut self, id: JobId) {
        if let Some(retry) = self.retries.remove(&id) {
            retry.timer.abort();
        }
    }

    /// Queues the job again, clearing its error.
    fn requeue(&mut self, id: JobId) -> Result<(), CoreError> {
        self.store.requeue(id)?;
        if let Some(view) = write(&self.views).get_mut(&id) {
            view.error = None;
        }
        self.set_view_state(id, JobState::Queued);
        Ok(())
    }

    fn state_of(&self, id: JobId) -> Option<JobState> {
        read(&self.views).get(&id).map(|v| v.state)
    }

    /// Sets the state in the job's view and announces it.
    fn set_view_state(&self, id: JobId, state: JobState) {
        let view = write(&self.views).get_mut(&id).map(|view| {
            view.state = state;
            view.speed_bps = 0;
            view.clone()
        });
        if let Some(view) = view {
            self.emit(Event::JobUpdated(view));
        }
    }

    /// Handles job messages until none of `ids` is running.
    async fn settle(&mut self, ids: &[JobId]) {
        while ids.iter().any(|id| self.running.contains_key(id)) {
            match self.job_rx.recv().await {
                Some(msg) => self.on_job_msg(msg),
                None => break,
            }
        }
    }

    /// Spawns the job's download task; the job is Downloading from here on.
    fn start(&mut self, id: JobId) {
        if let Err(e) = self.store.set_state(id, JobState::Downloading) {
            // The run records its own state as it starts.
            tracing::warn!(job = id.0, "could not record the start: {e}");
        }
        let downloaded_at_start = read(&self.views).get(&id).map_or(0, |v| v.downloaded);
        let cancel = CancellationToken::new();
        let (progress, mut progress_rx) = mpsc::unbounded_channel();
        let ctx = JobContext {
            id,
            client: self.client.clone(),
            store: self.store.clone(),
            cancel: cancel.clone(),
            progress,
            limiter: self.limiter.clone(),
            timing: self.timing.clone(),
            auto_checksum: self.settings.auto_checksum,
        };
        let tx = self.job_tx.clone();
        tokio::spawn(async move {
            // A task of its own, so a panic still ends in `Finished`.
            let job = tokio::spawn(run_job(ctx));
            // Ends when the job drops its progress sender, i.e. when it ends.
            let forward = async {
                while let Some((id, downloaded, total, speed_bps)) = progress_rx.recv().await {
                    let _ = tx.send(JobMsg::Progress {
                        id,
                        downloaded,
                        total,
                        speed_bps,
                    });
                }
            };
            let (joined, ()) = tokio::join!(job, forward);
            let outcome = joined.unwrap_or_else(|e| {
                JobOutcome::Failed(CoreError {
                    kind: ErrorKind::Unknown,
                    detail: format!("download task crashed: {e}"),
                })
            });
            let _ = tx.send(JobMsg::Finished { id, outcome });
        });
        self.running.insert(
            id,
            Run {
                cancel,
                downloaded_at_start,
            },
        );
        self.set_view_state(id, JobState::Downloading);
    }

    fn on_job_msg(&mut self, msg: JobMsg) {
        match msg {
            JobMsg::Progress {
                id,
                downloaded,
                total,
                speed_bps,
            } => {
                let before = write(&self.views).get_mut(&id).map(|view| {
                    view.total = total;
                    view.speed_bps = speed_bps;
                    std::mem::replace(&mut view.downloaded, downloaded)
                });
                if let Some(before) = before {
                    // A drop (a restarted stream) downloaded nothing.
                    self.measure(downloaded.saturating_sub(before));
                }
                self.emit(Event::Progress {
                    id,
                    downloaded,
                    total,
                    speed_bps,
                });
            }
            JobMsg::Finished { id, outcome } => self.on_finished(id, outcome),
            JobMsg::RetryDue { id, seq } => self.on_retry_due(id, seq),
        }
    }

    /// Refreshes the view from the store (finishing may have renamed the
    /// file) and announces it. A job that failed with an error that may pass
    /// (`is_transient`) waits to be retried instead, keeping the error.
    fn on_finished(&mut self, id: JobId, outcome: JobOutcome) {
        let run = self.running.remove(&id);
        let mut view = load_view(&self.store, id).unwrap_or_else(|e| {
            tracing::warn!(job = id.0, "could not reload the finished job: {e}");
            read(&self.views).get(&id).cloned()
        });
        let state = match outcome {
            JobOutcome::Completed(_) => {
                self.retry_counts.remove(&id);
                JobState::Completed
            }
            JobOutcome::Paused => JobState::Paused,
            JobOutcome::Failed(e) => {
                let state = if e.kind.is_transient() {
                    JobState::Waiting
                } else {
                    JobState::Failed
                };
                if let Some(view) = &mut view {
                    view.error = Some((e.kind, e.detail));
                }
                state
            }
        };
        if let Some(mut view) = view {
            if state == JobState::Waiting {
                if let Err(e) = self.store.set_state(id, JobState::Waiting) {
                    tracing::warn!(job = id.0, "could not record the wait: {e}");
                }
                // On shutdown, or without autostart, it stays Waiting.
                if !self.stopping && self.autostart {
                    let progressed =
                        run.is_some_and(|run| view.downloaded > run.downloaded_at_start);
                    self.retry_later(id, progressed);
                }
            }
            view.state = state;
            view.speed_bps = 0;
            write(&self.views).insert(id, view.clone());
            self.emit(Event::JobUpdated(view));
        }
        self.after_change();
    }

    /// Counts newly downloaded bytes and, while unlimited, saves a new peak.
    /// The tracker starts from the saved peak when that is fresh, so every
    /// peak it reports beats the saved one or replaces a stale one.
    fn measure(&mut self, new_bytes: u64) {
        self.transferred = self.transferred.saturating_add(new_bytes);
        if self.settings.speed != SpeedLimit::Fastest {
            return;
        }
        let Some(bps) = self.peak.record(Instant::now(), self.transferred) else {
            return;
        };
        let at = unix_now();
        match self.store.save_measured_peak(bps, at) {
            Ok(()) => self.measured_peak = Some((bps, at)),
            Err(e) => tracing::warn!("could not save the peak speed: {e}"),
        }
    }

    fn emit(&self, event: Event) {
        // No subscribers is fine.
        let _ = self.events.send(event);
    }
}

/// Whether a job in `state` is done until someone acts on it: nothing will
/// run it on its own.
fn is_final(state: JobState) -> bool {
    matches!(
        state,
        JobState::Completed | JobState::Paused | JobState::Failed
    )
}

/// Delay before a Waiting job's retry after `count` earlier waits in a row:
/// `waiting_retry_first` doubled `count` times, at most `waiting_retry_cap`.
fn retry_delay(timing: &Timing, count: u32) -> Duration {
    timing
        .waiting_retry_first
        .saturating_mul(2u32.saturating_pow(count))
        .min(timing.waiting_retry_cap)
}

/// The limiter rate for `settings`: presets are shares of the saved peak.
fn speed_limit(settings: &Settings, measured_peak: Option<(u64, i64)>) -> Option<u64> {
    settings
        .speed
        .bytes_per_sec(measured_peak.map(|(bps, _)| bps))
}

/// The saved peak, unless it is older than 7 days.
fn fresh_peak(measured_peak: Option<(u64, i64)>, now: i64) -> Option<u64> {
    measured_peak
        .filter(|&(_, at)| now.saturating_sub(at) <= PEAK_MAX_AGE_SECS)
        .map(|(bps, _)| bps)
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// The URL after the host's link fixups; an unparseable URL as is, for
/// `resolve` to report.
fn fixed_url(url: &str) -> String {
    match url::Url::parse(url) {
        Ok(parsed) => fixups::rewrite(&parsed).unwrap_or(parsed).to_string(),
        Err(_) => url.to_owned(),
    }
}

/// The mirrors that serve the primary's file, each as the link to request
/// (a confirm form's, if one was followed). A mirror that fails to resolve,
/// serves another size, or whose validators say it serves another version
/// (the rule segment responses follow) is dropped.
async fn vet_mirrors(
    client: &reqwest::Client,
    primary: &Probe,
    mirrors: &[String],
    referer: Option<&str>,
) -> Vec<String> {
    let mut kept = Vec::new();
    for mirror in mirrors {
        let why = match resolve(client, &fixed_url(mirror), referer).await {
            Err(e) => e.to_string(),
            Ok(probe) if probe.size != primary.size => format!(
                "size {} differs from {}",
                describe_size(probe.size),
                describe_size(primary.size)
            ),
            Ok(probe) => match mirror_change(primary, &probe) {
                Some(change) => change,
                None => {
                    kept.push(probe.requested_url.to_string());
                    continue;
                }
            },
        };
        tracing::info!(mirror = loggable(mirror), "dropping the mirror: {why}");
    }
    kept
}

/// How the mirror's validators differ from the primary's, as for a 206.
fn mirror_change(primary: &Probe, mirror: &Probe) -> Option<String> {
    validator_change(
        (primary.etag.as_deref(), primary.last_modified.as_deref()),
        (mirror.etag.as_deref(), mirror.last_modified.as_deref()),
    )
    .map(|what| format!("{what} (primary to mirror)"))
}

/// Copies what the probe learned into the record.
fn apply_probe(job: &mut JobRecord, probe: &Probe) {
    job.final_url = Some(probe.final_url.to_string());
    job.total = probe.size;
    job.resumable = probe.ranges && probe.size.is_some();
    job.etag = probe.etag.clone();
    job.last_modified = probe.last_modified.clone();
}

/// What says the file on the server is no longer the one partly downloaded:
/// the validators (see `validator_change`), or the size.
fn change_on_server(job: &JobRecord, probe: &Probe) -> Option<String> {
    let validator = validator_change(
        (job.etag.as_deref(), job.last_modified.as_deref()),
        (probe.etag.as_deref(), probe.last_modified.as_deref()),
    );
    validator.or_else(|| {
        (job.total != probe.size).then(|| {
            format!(
                "size changed from {} to {}",
                describe_size(job.total),
                describe_size(probe.size)
            )
        })
    })
}

fn describe_size(size: Option<u64>) -> String {
    size.map_or_else(|| "unknown".to_string(), |s| format!("{s} bytes"))
}

fn view_of(job: &JobRecord, downloaded: u64) -> JobView {
    JobView {
        id: job.id,
        url: job.url.clone(),
        file_name: job.file_name.clone(),
        dest_dir: job.dest_dir.clone(),
        state: job.state,
        total: job.total,
        downloaded,
        speed_bps: 0,
        resumable: job.resumable,
        error: job.error.clone(),
    }
}

fn load_view(store: &Store, id: JobId) -> Result<Option<JobView>, CoreError> {
    let Some(job) = store.get_job(id)? else {
        return Ok(None);
    };
    Ok(Some(view_of(&job, downloaded(store, id)?)))
}

/// Durable progress: the sum of the stored segments' `done`.
fn downloaded(store: &Store, id: JobId) -> Result<u64, CoreError> {
    Ok(store.load_segments(id)?.iter().map(|s| s.done).sum())
}

fn partial_path(job: &JobRecord) -> PathBuf {
    job.dest_dir
        .join(format!("{}.{PARTIAL_EXT}", job.file_name))
}

/// Deletes the job's partial file and, once completed, its downloaded file.
/// Spares both if another job is recorded under the same name, and spares a
/// downloaded file whose size is no longer the downloaded size.
fn delete_job_files(store: &Store, job: &JobRecord) -> Result<(), CoreError> {
    if store.name_used_by_other(job.id, &job.dest_dir, &job.file_name)? {
        return Ok(());
    }
    remove_if_exists(&partial_path(job))?;
    if job.state == JobState::Completed {
        let path = job.dest_dir.join(&job.file_name);
        let ours = std::fs::metadata(&path)
            .is_ok_and(|m| m.is_file() && job.total.is_none_or(|total| m.len() == total));
        if ours {
            remove_if_exists(&path)?;
        }
    }
    Ok(())
}

fn remove_if_exists(path: &Path) -> Result<(), CoreError> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(io_error(e)),
        _ => Ok(()),
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

/// Takes the exclusive lock on `data_dir/taurus.lock` without waiting.
fn lock_data_dir(data_dir: &Path) -> Result<std::fs::File, CoreError> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(data_dir.join(LOCK_FILE))
        .map_err(io_error)?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(std::fs::TryLockError::WouldBlock) => Err(CoreError {
            kind: ErrorKind::Unknown,
            detail: format!("{IN_USE} {}", data_dir.display()),
        }),
        Err(std::fs::TryLockError::Error(e)) => Err(io_error(e)),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_delay_doubles_up_to_the_cap() {
        let timing = Timing::default();
        let delays: Vec<u64> = [0, 1, 2, 3, 4, 5, 40]
            .into_iter()
            .map(|count| retry_delay(&timing, count).as_secs())
            .collect();
        assert_eq!(delays, [60, 120, 240, 480, 600, 600, 600]);
    }

    #[test]
    fn a_peak_older_than_seven_days_is_not_fresh() {
        let now = 1_700_000_000;
        let week = 7 * 24 * 60 * 60;
        assert_eq!(fresh_peak(None, now), None);
        assert_eq!(fresh_peak(Some((5, now - 3600)), now), Some(5));
        assert_eq!(fresh_peak(Some((5, now - week)), now), Some(5));
        assert_eq!(fresh_peak(Some((5, now - week - 1)), now), None);
    }
}
