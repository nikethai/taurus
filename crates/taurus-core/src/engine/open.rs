//! Opening an engine: taking the data dir, restoring the stored jobs and
//! assembling the actor.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::sync::{Arc, RwLock};

use tokio::sync::{broadcast, mpsc, watch};

use crate::errors::{CoreError, ErrorKind};
use crate::http::build_client;
use crate::limit::{Limiter, PeakTracker};
use crate::store::{JobKind, JobState, Store};
use crate::system::AwakeGuard;
use crate::torrent::hub::TorrentHub;
use crate::when_done::{EngineLink, WhenDoneRunner};

use super::actor::{Actor, Command, JobMsg};
use super::jobs::{downloaded, view_of};
use super::settings::{fresh_peak, speed_limit, unix_now};
use super::{
    COMMAND_CAPACITY, DB_FILE, EVENT_CAPACITY, Engine, EngineConfig, IN_USE, JobView, LOCK_FILE,
    io_error,
};

/// Everything `Engine::open` does but starting the actor.
pub(super) async fn assemble(
    config: EngineConfig,
) -> Result<(Engine, Actor, mpsc::Receiver<Command>), CoreError> {
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
            // Interrupted, not paused: it runs again, in its place, once
            // checked against the server (an HTTP job; see
            // `Store::requeue_interrupted`).
            JobState::Downloading | JobState::Seeding => {
                store.requeue_interrupted(job.id)?;
                job.state = JobState::Queued;
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

    let hub = Arc::new(TorrentHub::new(
        &config.data_dir,
        &settings.torrent,
        config.torrent_test.clone(),
        store.clone(),
    ));
    // Torrents about to run start the session now, which makes it agree
    // with the store first. Short-lived front ends that only look at or
    // edit the jobs (no autostart) leave it for the first torrent that runs.
    let torrents_queued = views
        .values()
        .any(|v: &JobView| v.kind == JobKind::Torrent && v.state == JobState::Queued);
    if config.autostart
        && torrents_queued
        && let Err(e) = hub.session().await
    {
        tracing::warn!("could not start the torrent session: {e}");
    }
    let views = Arc::new(RwLock::new(views));
    let shared_settings = Arc::new(RwLock::new(settings.clone()));
    let keeping_awake = Arc::new(AtomicBool::new(false));
    let limiter = Limiter::new(speed_limit(&settings, measured_peak));
    let peak = PeakTracker::with_max(fresh_peak(measured_peak, unix_now()).unwrap_or(0));
    let (commands, commands_rx) = mpsc::channel(COMMAND_CAPACITY);
    let (events, _) = broadcast::channel(EVENT_CAPACITY);
    let (job_tx, job_rx) = mpsc::unbounded_channel();
    let holds = Arc::new(AtomicUsize::new(0));
    let on_fired = {
        let job_tx: mpsc::UnboundedSender<JobMsg> = job_tx.clone();
        Arc::new(move |action| {
            let _ = job_tx.send(JobMsg::WhenDoneFired(action));
        })
    };
    let client = build_client();
    let (torrent_settings, _) = watch::channel(settings.torrent.clone());
    let actor = Actor {
        _lock: lock,
        store,
        client: client.clone(),
        views: views.clone(),
        events: events.clone(),
        running: HashMap::new(),
        waiters: HashMap::new(),
        retries: HashMap::new(),
        retry_counts: HashMap::new(),
        resolved_again: HashMap::new(),
        next_retry_seq: 0,
        busy: false,
        job_ended: false,
        idle_pending: false,
        holds: holds.clone(),
        stopping: false,
        autostart: config.autostart,
        space: config.available_space,
        when_done: WhenDoneRunner::new(
            events.clone(),
            config.power,
            config.timing.when_done_countdown,
            EngineLink {
                on_fired,
                holds: holds.clone(),
            },
        ),
        awake: AwakeGuard::new(),
        keeping_awake: keeping_awake.clone(),
        job_tx: job_tx.clone(),
        job_rx,
        timing: config.timing.clone(),
        settings,
        shared_settings: shared_settings.clone(),
        limiter,
        hub: hub.clone(),
        torrent_settings,
        rates: None,
        measured_peak,
        peak,
        transferred: 0,
    };
    let engine = Engine {
        commands,
        events,
        views,
        settings: shared_settings,
        keeping_awake,
        client,
        holds,
        hold_tx: job_tx,
        space: config.available_space,
        hub,
        metadata_timeout: config.timing.torrent_metadata_timeout,
    };
    Ok((engine, actor, commands_rx))
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
