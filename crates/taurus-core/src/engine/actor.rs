//! The actor: the one task that owns the running jobs, the commands it
//! serves and the messages its jobs and timers send it.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};

use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio::task::AbortHandle;
use tokio_util::sync::CancellationToken;

use crate::errors::CoreError;
use crate::http::{JobOutcome, Probe};
use crate::limit::{Limiter, PeakTracker};
use crate::secrets::Credentials;
use crate::settings::{Settings, TorrentSettings, WhenDone};
use crate::store::{JobId, JobKind, JobRecord, JobState, Store};
use crate::system::{AwakeGuard, SpaceSource};
use crate::timing::Timing;
use crate::tools::ToolManager;
use crate::torrent::hub::TorrentHub;
use crate::when_done::WhenDoneRunner;

use super::keychain::Secrets;
use super::media::{MediaAdd, MediaUpdate};
use super::torrent::TorrentInput;
use super::{AddRequest, Event, RemoveFiles, TorrentView, Views, no_such_job, read, write};

pub(super) type Reply<T> = oneshot::Sender<Result<T, CoreError>>;

pub(super) enum Command {
    Add {
        req: AddRequest,
        probe: Probe,
        reply: Reply<JobId>,
    },
    AddTorrent {
        req: AddRequest,
        input: TorrentInput,
        reply: Reply<JobId>,
    },
    AddMedia {
        req: AddRequest,
        add: MediaAdd,
        reply: Reply<JobId>,
    },
    /// Answers whether the job runs and was asked to stop.
    StopRecording {
        id: JobId,
        reply: Reply<bool>,
    },
    Pause {
        id: JobId,
        reply: Reply<()>,
    },
    /// `probe` is `None` for a torrent.
    Resume {
        id: JobId,
        probe: Option<Probe>,
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
    /// Answers the job as it was.
    SetCredentials {
        id: JobId,
        creds: Credentials,
        reply: Reply<JobRecord>,
    },
    SignInFailed {
        id: JobId,
        error: CoreError,
        reply: oneshot::Sender<()>,
    },
    Remove {
        id: JobId,
        files: RemoveFiles,
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
/// or by a Waiting job's retry timer. Public only for the torrent runner's
/// tests; not for front ends.
#[doc(hidden)]
pub enum JobMsg {
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
    /// The torrent finished downloading and is sharing (sent once per run,
    /// never when the seeding policy stops it as soon as it finishes).
    /// `finished_now`: the download finished in this run, rather than in an
    /// earlier one (it shares again after a restart).
    Seeding {
        id: JobId,
        finished_now: bool,
    },
    /// A running torrent's peers, uploads and file selection, with each
    /// `Progress`.
    TorrentStats {
        id: JobId,
        view: TorrentView,
    },
    /// Timer `seq` of the job ran out.
    RetryDue {
        id: JobId,
        seq: u64,
    },
    /// A Sleep or Shutdown fired.
    WhenDoneFired(WhenDone),
    /// A `WhenDoneHold` was taken or dropped.
    HoldChanged,
    /// A running media job's phase, entries or files changed.
    Media {
        id: JobId,
        update: MediaUpdate,
    },
    /// A media job's thumbnail was downloaded.
    Thumbnail {
        id: JobId,
        path: std::path::PathBuf,
    },
    /// The job's link as added was resolved again after its link expired;
    /// `None` if that was stopped (pause, removal, shutdown).
    Resolved {
        id: JobId,
        probe: Option<Result<Box<Probe>, CoreError>>,
    },
}

pub(super) struct Actor {
    /// Holds the data dir; released when the actor stops.
    pub(super) _lock: std::fs::File,
    pub(super) store: Store,
    pub(super) client: reqwest::Client,
    pub(super) views: Views,
    pub(super) events: broadcast::Sender<Event>,
    /// The jobs whose download task is running.
    pub(super) running: HashMap<JobId, Run>,
    /// `wait` callers of jobs that are not Completed, Paused or Failed yet.
    pub(super) waiters: HashMap<JobId, Vec<oneshot::Sender<JobState>>>,
    /// Retry timers of Waiting jobs.
    pub(super) retries: HashMap<JobId, Retry>,
    /// Waits in a row without progress, per job: the next wait doubles.
    pub(super) retry_counts: HashMap<JobId, u32>,
    /// Jobs whose link was resolved again after it expired, with their
    /// durable progress then: once more only after progress past it, or
    /// after the user resumes the job.
    pub(super) resolved_again: HashMap<JobId, u64>,
    pub(super) next_retry_seq: u64,
    /// Whether a job was Downloading, Queued or Waiting at the last change.
    pub(super) busy: bool,
    /// A run ended Completed or Failed since the last change: only that
    /// makes an idle queue run the when-done action.
    pub(super) job_ended: bool,
    /// The queue went idle for the when-done action while it was held.
    pub(super) idle_pending: bool,
    /// Live `WhenDoneHold`s.
    pub(super) holds: Arc<AtomicUsize>,
    /// Shutting down: start nothing more.
    pub(super) stopping: bool,
    /// Whether jobs start and retry on their own; else only `start_now`.
    pub(super) autostart: bool,
    /// Where free space is read from.
    pub(super) space: SpaceSource,
    pub(super) when_done: WhenDoneRunner,
    pub(super) awake: AwakeGuard,
    pub(super) keeping_awake: Arc<AtomicBool>,
    pub(super) job_tx: mpsc::UnboundedSender<JobMsg>,
    pub(super) job_rx: mpsc::UnboundedReceiver<JobMsg>,
    pub(super) timing: Timing,
    pub(super) settings: Settings,
    pub(super) shared_settings: Arc<RwLock<Settings>>,
    /// Shared by every HTTP job's workers.
    pub(super) limiter: Limiter,
    /// The engine's torrent session (started on first use).
    pub(super) hub: Arc<TorrentHub>,
    /// The torrent settings each torrent runner reads its seeding policy
    /// from.
    pub(super) torrent_settings: watch::Sender<TorrentSettings>,
    /// The rates last applied: HTTP download, torrent download and upload.
    pub(super) rates: Option<Rates>,
    /// The saved peak speed (bytes/s) and when it was measured (unix secs).
    pub(super) measured_peak: Option<(u64, i64)>,
    pub(super) peak: PeakTracker,
    /// Bytes downloaded by all jobs since the engine opened.
    pub(super) transferred: u64,
    /// yt-dlp, ffmpeg and Deno, for media jobs.
    pub(super) tools: Arc<ToolManager>,
    /// The `stop_recording` token of each running media job.
    pub(super) media_stops: HashMap<JobId, CancellationToken>,
    /// The jobs' passwords.
    pub(super) secrets: Secrets,
}

pub(super) type Rates = (Option<u64>, Option<u64>, Option<u64>);

pub(super) struct Run {
    pub(super) cancel: CancellationToken,
    /// Durable progress when the run started.
    pub(super) downloaded_at_start: u64,
}

pub(super) struct Retry {
    /// Tells this timer's `RetryDue` from an older, cancelled one's.
    pub(super) seq: u64,
    pub(super) timer: AbortHandle,
}

impl Actor {
    /// Serves commands until `shutdown` or until every handle is dropped.
    pub(super) async fn run(mut self, mut commands: mpsc::Receiver<Command>) {
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
        // Includes a when-done that fired just before: whoever heard of it
        // may be what stopped the engine, and it must still be turned off.
        while let Ok(msg) = self.job_rx.try_recv() {
            self.on_job_msg(msg);
        }
        // Running jobs end Queued too: see `on_finished`.
        self.stop_all().await;
        self.hub.shutdown().await;
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
            Command::Add { .. }
                | Command::AddTorrent { .. }
                | Command::AddMedia { .. }
                | Command::Resume { .. }
                | Command::StartNow { .. }
        ) {
            self.when_done.cancel();
        }
        match command {
            Command::Add { req, probe, reply } => {
                let _ = reply.send(self.add(req, &probe));
            }
            Command::AddTorrent { req, input, reply } => {
                let _ = reply.send(self.add_torrent(req, input));
            }
            Command::AddMedia { req, add, reply } => {
                let _ = reply.send(self.add_media(req, add));
            }
            Command::StopRecording { id, reply } => {
                let _ = reply.send(self.stop_recording(id));
            }
            Command::Pause { id, reply } => {
                let _ = reply.send(self.pause(id).await);
            }
            Command::Resume { id, probe, reply } => {
                let _ = reply.send(self.resume(id, probe.as_ref()));
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
            Command::SetCredentials { id, creds, reply } => {
                let _ = reply.send(self.set_credentials(id, creds));
            }
            Command::SignInFailed { id, error, reply } => {
                self.sign_in_failed(id, error);
                let _ = reply.send(());
            }
            Command::Remove { id, files, reply } => {
                let _ = reply.send(self.remove(id, files).await);
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

    fn wait(&mut self, id: JobId, reply: oneshot::Sender<JobState>) {
        match self.state_of(id) {
            Some(state) if !is_final(state) => self.waiters.entry(id).or_default().push(reply),
            state => {
                let _ = reply.send(state.unwrap_or(JobState::Failed));
            }
        }
    }

    async fn stop_all(&mut self) {
        let ids: Vec<JobId> = self.running.keys().copied().collect();
        for run in self.running.values() {
            run.cancel.cancel();
        }
        self.settle(&ids).await;
    }

    pub(super) fn state_of(&self, id: JobId) -> Option<JobState> {
        read(&self.views).get(&id).map(|v| v.state)
    }

    /// Sets the state in the job's view and announces it.
    pub(super) fn set_view_state(&self, id: JobId, state: JobState) {
        let view = write(&self.views).get_mut(&id).map(|view| {
            view.state = state;
            view.speed_bps = 0;
            // Only a Waiting job has a retry ahead; callers that make a job
            // Waiting set it themselves.
            view.retry_at = None;
            view.clone()
        });
        if let Some(view) = view {
            self.emit(Event::JobUpdated(view));
        }
    }

    /// Handles job messages until none of `ids` is running.
    pub(super) async fn settle(&mut self, ids: &[JobId]) {
        while ids.iter().any(|id| self.running.contains_key(id)) {
            match self.job_rx.recv().await {
                Some(msg) => self.on_job_msg(msg),
                None => break,
            }
        }
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
                    let before = std::mem::replace(&mut view.downloaded, downloaded);
                    (view.kind, before)
                });
                // The peak is the connection's: a torrent's speed is its
                // peers', and its first progress is what is already on disk.
                if let Some((JobKind::Http, before)) = before {
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
            JobMsg::Seeding { id, finished_now } => self.on_seeding(id, finished_now),
            JobMsg::TorrentStats { id, view } => self.on_torrent_stats(id, view),
            JobMsg::RetryDue { id, seq } => self.on_retry_due(id, seq),
            JobMsg::WhenDoneFired(action) => self.on_when_done_fired(action),
            JobMsg::Resolved { id, probe } => self.on_resolved(id, probe),
            JobMsg::Media { id, update } => self.on_media_update(id, update),
            JobMsg::Thumbnail { id, path } => self.on_thumbnail(id, path),
            JobMsg::HoldChanged => {
                if self.holds.load(Ordering::SeqCst) > 0 {
                    self.when_done.cancel();
                } else {
                    self.after_change();
                }
            }
        }
    }

    pub(super) fn emit(&self, event: Event) {
        // No subscribers is fine.
        let _ = self.events.send(event);
    }
}

/// Whether a job in `state` is done until someone acts on it: nothing will
/// run it on its own. Seeding is not final: the job is still sharing.
pub(super) fn is_final(state: JobState) -> bool {
    matches!(
        state,
        JobState::Completed | JobState::Paused | JobState::Failed
    )
}
