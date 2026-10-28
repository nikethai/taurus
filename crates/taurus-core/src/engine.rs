//! The public entry point: a cheap-to-clone [`Engine`] handle over one
//! background actor that owns the running jobs, restores jobs from the store
//! on open and broadcasts [`Event`]s.
//!
//! Network probes run in the caller's task; everything that touches the
//! running jobs or reserves names runs in the actor, one command at a time.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::errors::{CoreError, ErrorKind, classify_io};
use crate::filename::{PARTIAL_EXT, sanitize, unique_path};
use crate::fixups;
use crate::http::{JobContext, JobOutcome, Probe, build_client, resolve, run_job};
use crate::limit::{Limiter, PeakTracker};
use crate::plan::DEFAULT_CONNECTIONS;
use crate::settings::{Settings, SpeedLimit};
use crate::store::{JobId, JobRecord, JobState, NewJob, Store};
use crate::timing::Timing;

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
}

pub struct AddRequest {
    pub url: String,
    pub dest_dir: PathBuf,
    /// Overrides the name the server suggests; sanitized before use.
    pub file_name: Option<String>,
    /// Defaults to `DEFAULT_CONNECTIONS`.
    pub connections: Option<u32>,
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
    Shutdown {
        reply: oneshot::Sender<()>,
    },
}

/// Sent by a job's task. All of a run's progress precedes its `Finished`.
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
}

impl Engine {
    /// Opens the database in `config.data_dir`, restores its jobs and applies
    /// the saved settings. Jobs recorded as Downloading (interrupted) or
    /// Queued become Paused.
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
        let settings = store.load_settings()?;
        // Only a measurement: an unreadable one is measured again.
        let measured_peak = store.measured_peak().unwrap_or_else(|e| {
            tracing::warn!("ignoring the saved peak speed: {e}");
            None
        });
        let mut views = HashMap::new();
        for mut job in store.list_jobs()? {
            // Nothing is running yet, and there is no queue to start Queued or
            // Waiting jobs.
            if matches!(
                job.state,
                JobState::Downloading | JobState::Queued | JobState::Waiting
            ) {
                job.state = JobState::Paused;
                store.update_job(&job)?;
            }
            let downloaded = downloaded(&store, job.id)?;
            views.insert(job.id, view_of(&job, downloaded));
        }

        let views = Arc::new(RwLock::new(views));
        let shared_settings = Arc::new(RwLock::new(settings.clone()));
        let limiter = Limiter::new(speed_limit(&settings, measured_peak));
        let peak = PeakTracker::with_max(fresh_peak(measured_peak, unix_now()).unwrap_or(0));
        let (commands, commands_rx) = mpsc::channel(COMMAND_CAPACITY);
        let (events, _) = broadcast::channel(EVENT_CAPACITY);
        let (job_tx, job_rx) = mpsc::unbounded_channel();
        let client = build_client();
        let actor = Actor {
            _lock: lock,
            store,
            client: client.clone(),
            views: views.clone(),
            events: events.clone(),
            running: HashMap::new(),
            waiters: HashMap::new(),
            job_tx,
            job_rx,
            settings,
            shared_settings: shared_settings.clone(),
            limiter,
            measured_peak,
            peak,
            transferred: 0,
        };
        tokio::spawn(actor.run(commands_rx));
        Ok(Engine {
            commands,
            events,
            views,
            settings: shared_settings,
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
    /// page or bot check), reserves a file name that no other file or job in
    /// `dest_dir` uses, and starts downloading. A missing `dest_dir` fails
    /// with `FolderUnavailable`; nothing is recorded when `add` fails.
    pub async fn add(&self, req: AddRequest) -> Result<JobId, CoreError> {
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
        // An unparseable URL goes to `resolve` as is, which reports it.
        let url = match url::Url::parse(&req.url) {
            Ok(parsed) => fixups::rewrite(&parsed).unwrap_or(parsed).to_string(),
            Err(_) => req.url.clone(),
        };
        let probe = resolve(&self.client, &url, None).await?;
        self.call(|reply| Command::Add { req, probe, reply })
            .await?
    }

    /// Stops the job and returns once its progress is saved and its state is
    /// Paused. Pausing a job that is not running does nothing.
    pub async fn pause(&self, id: JobId) -> Result<(), CoreError> {
        self.call(|reply| Command::Pause { id, reply }).await?
    }

    /// Re-resolves the job's URL and continues it. If the file changed on the
    /// server (ETag, else Last-Modified, or size) or the download cannot be
    /// resumed, it restarts from zero; a change is noted as the job's error
    /// (`FileChangedOnServer`) while it downloads. Resuming a running or
    /// completed job does nothing. A failed probe is returned and leaves the
    /// job as it was.
    pub async fn resume(&self, id: JobId) -> Result<(), CoreError> {
        self.check_running()?;
        let url = match self.view(id) {
            None => return Err(no_such_job()),
            Some(v) if matches!(v.state, JobState::Downloading | JobState::Completed) => {
                return Ok(());
            }
            Some(v) => v.url,
        };
        let probe = resolve(&self.client, &url, None).await?;
        self.call(|reply| Command::Resume { id, probe, reply })
            .await?
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
    /// then emits `SettingsChanged`. Nothing changes if saving fails.
    pub async fn set_settings(&self, settings: Settings) -> Result<(), CoreError> {
        self.call(|reply| Command::SetSettings { settings, reply })
            .await?
    }

    /// Resolves once the job is Completed, Paused or Failed (at once if it
    /// already is). An unknown job resolves to Failed immediately.
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
    /// Cancel tokens of the jobs whose download task is running.
    running: HashMap<JobId, CancellationToken>,
    /// `wait` callers of running jobs.
    waiters: HashMap<JobId, Vec<oneshot::Sender<JobState>>>,
    job_tx: mpsc::UnboundedSender<JobMsg>,
    job_rx: mpsc::UnboundedReceiver<JobMsg>,
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
            mirrors: Vec::new(),
            checksum: None,
            referer: None,
            queue_pos: 0,
        })?;
        let mut job = JobRecord {
            id,
            url,
            final_url: None,
            dest_dir: req.dest_dir,
            file_name,
            state: JobState::Downloading,
            total: None,
            resumable: false,
            etag: None,
            last_modified: None,
            connections,
            error: None,
            mirrors: Vec::new(),
            checksum: None,
            referer: None,
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
        self.start(id);
        Ok(id)
    }

    async fn pause(&mut self, id: JobId) -> Result<(), CoreError> {
        if !read(&self.views).contains_key(&id) {
            return Err(no_such_job());
        }
        if let Some(cancel) = self.running.get(&id) {
            cancel.cancel();
            self.settle(&[id]).await;
        }
        Ok(())
    }

    fn resume(&mut self, id: JobId, probe: &Probe) -> Result<(), CoreError> {
        let mut job = self.store.get_job(id)?.ok_or_else(no_such_job)?;
        if self.running.contains_key(&id) || job.state == JobState::Completed {
            return Ok(());
        }
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
        job.state = JobState::Downloading;
        self.store.update_job(&job)?;
        let view = view_of(&job, downloaded(&self.store, id)?);
        write(&self.views).insert(id, view.clone());
        self.emit(Event::JobUpdated(view));
        self.start(id);
        Ok(())
    }

    async fn remove(&mut self, id: JobId, delete_files: bool) -> Result<(), CoreError> {
        if !read(&self.views).contains_key(&id) {
            return Err(no_such_job());
        }
        if let Some(cancel) = self.running.get(&id) {
            cancel.cancel();
            self.settle(&[id]).await;
        }
        if delete_files && let Some(job) = self.store.get_job(id)? {
            delete_job_files(&self.store, &job)?;
        }
        self.store.delete_job(id)?;
        write(&self.views).remove(&id);
        self.emit(Event::JobRemoved(id));
        Ok(())
    }

    fn wait(&mut self, id: JobId, reply: oneshot::Sender<JobState>) {
        if self.running.contains_key(&id) {
            self.waiters.entry(id).or_default().push(reply);
            return;
        }
        let state = read(&self.views).get(&id).map(|v| v.state);
        let _ = reply.send(state.unwrap_or(JobState::Failed));
    }

    fn set_settings(&mut self, settings: Settings) -> Result<(), CoreError> {
        self.store.save_settings(&settings)?;
        self.limiter
            .set_rate(speed_limit(&settings, self.measured_peak));
        *write(&self.shared_settings) = settings.clone();
        self.settings = settings.clone();
        self.emit(Event::SettingsChanged(settings));
        Ok(())
    }

    async fn pause_all(&mut self) {
        let ids: Vec<JobId> = self.running.keys().copied().collect();
        for cancel in self.running.values() {
            cancel.cancel();
        }
        self.settle(&ids).await;
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

    /// Spawns the job's download task.
    fn start(&mut self, id: JobId) {
        let cancel = CancellationToken::new();
        let (progress, mut progress_rx) = mpsc::unbounded_channel();
        let ctx = JobContext {
            id,
            client: self.client.clone(),
            store: self.store.clone(),
            cancel: cancel.clone(),
            progress,
            limiter: self.limiter.clone(),
            timing: Timing::default(),
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
        self.running.insert(id, cancel);
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
        }
    }

    /// Refreshes the view from the store (finishing may have renamed the
    /// file), announces it and answers the job's waiters.
    fn on_finished(&mut self, id: JobId, outcome: JobOutcome) {
        self.running.remove(&id);
        let mut view = load_view(&self.store, id).unwrap_or_else(|e| {
            tracing::warn!(job = id.0, "could not reload the finished job: {e}");
            read(&self.views).get(&id).cloned()
        });
        let state = match outcome {
            JobOutcome::Completed(_) => JobState::Completed,
            JobOutcome::Paused => JobState::Paused,
            JobOutcome::Failed(e) => {
                if let Some(view) = &mut view {
                    view.error = Some((e.kind, e.detail));
                }
                JobState::Failed
            }
        };
        if let Some(mut view) = view {
            view.state = state;
            view.speed_bps = 0;
            write(&self.views).insert(id, view.clone());
            self.emit(Event::JobUpdated(view));
        }
        for waiter in self.waiters.remove(&id).unwrap_or_default() {
            let _ = waiter.send(state);
        }
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

/// Copies what the probe learned into the record.
fn apply_probe(job: &mut JobRecord, probe: &Probe) {
    job.final_url = Some(probe.final_url.to_string());
    job.total = probe.size;
    job.resumable = probe.ranges && probe.size.is_some();
    job.etag = probe.etag.clone();
    job.last_modified = probe.last_modified.clone();
}

/// What says the file on the server is no longer the one partly downloaded:
/// the ETag, else (no ETag on both sides) Last-Modified, or the size.
fn change_on_server(job: &JobRecord, probe: &Probe) -> Option<String> {
    let validator = match (&job.etag, &probe.etag) {
        (Some(old), Some(new)) => (old != new).then(|| format!("ETag changed from {old} to {new}")),
        _ => match (&job.last_modified, &probe.last_modified) {
            (Some(old), Some(new)) if old != new => {
                Some(format!("Last-Modified changed from {old} to {new}"))
            }
            _ => None,
        },
    };
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
    fn a_peak_older_than_seven_days_is_not_fresh() {
        let now = 1_700_000_000;
        let week = 7 * 24 * 60 * 60;
        assert_eq!(fresh_peak(None, now), None);
        assert_eq!(fresh_peak(Some((5, now - 3600)), now), Some(5));
        assert_eq!(fresh_peak(Some((5, now - week)), now), Some(5));
        assert_eq!(fresh_peak(Some((5, now - week - 1)), now), None);
    }
}
