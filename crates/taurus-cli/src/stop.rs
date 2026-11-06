//! `hyades-cli stop <ID>`: ends a running video download or live recording and
//! keeps what was downloaded.
//!
//! The job runs in the process that holds the data dir (`add`, `run` or the
//! desktop app), and a second process cannot open an engine beside it. So
//! `stop` asks that engine through `Engine::request_stop` and waits for the
//! answer: the request being taken, then the job's end.

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use taurus_core::{Engine, EngineConfig, JobId, JobKind, JobState, JobView};

use crate::Result;

/// How long the running engine has to take the request (it polls four times
/// a second).
const ANSWER: Duration = Duration::from_secs(5);
/// How long a graceful stop may take: yt-dlp's wind-down, then ffmpeg making
/// a recording playable.
const WIND_DOWN: Duration = Duration::from_secs(60);
const POLL: Duration = Duration::from_millis(100);

/// The job, if it is one that can be stopped now.
fn stoppable(views: Vec<JobView>, id: JobId) -> Result<JobView> {
    let view = views
        .into_iter()
        .find(|v| v.id == id)
        .ok_or_else(|| format!("download {} not found", id.0))?;
    if view.kind != JobKind::Media {
        return Err(format!("download {} is not a video or recording", id.0).into());
    }
    if !running(&view) {
        return Err(format!("download {} is not recording", id.0).into());
    }
    Ok(view)
}

fn running(view: &JobView) -> bool {
    matches!(view.state, JobState::Recording | JobState::Downloading)
}

/// `stop` when no other process holds the data dir: nothing runs here, so
/// there is nothing to stop.
pub fn local(engine: &Engine, id: JobId) -> Result<ExitCode> {
    stoppable(engine.snapshot(), id)?;
    Err(format!("download {} is not recording", id.0).into())
}

/// Asks the engine that holds `data_dir` to stop job `id`, and reports how
/// the job ended (the job as JSON with `--json`).
pub async fn request(data_dir: PathBuf, id: JobId, json: bool) -> Result<ExitCode> {
    let config = EngineConfig::new(data_dir);
    stoppable(Engine::read_snapshot(&config)?, id)?;
    Engine::request_stop(&config, id)?;
    eprintln!("Asking the running Hyades to stop download {}...", id.0);
    let deadline = tokio::time::Instant::now() + ANSWER;
    while Engine::stop_request_pending(&config, id) {
        if tokio::time::Instant::now() >= deadline {
            Engine::withdraw_stop_request(&config, id);
            return Err(format!(
                "the running Hyades did not answer the request to stop download {} \
                 (it may be busy, or too old to take stop requests)",
                id.0
            )
            .into());
        }
        tokio::time::sleep(POLL).await;
    }
    let deadline = tokio::time::Instant::now() + WIND_DOWN;
    let view = loop {
        let view = Engine::read_snapshot(&config)?
            .into_iter()
            .find(|v| v.id == id)
            .ok_or_else(|| format!("download {} not found", id.0))?;
        if !running(&view) {
            break view;
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(format!(
                "download {} is still stopping after {} s",
                id.0,
                WIND_DOWN.as_secs()
            )
            .into());
        }
        tokio::time::sleep(POLL).await;
    };
    if json {
        println!("{}", serde_json::to_string(&view)?);
    }
    if view.state == JobState::Completed {
        if !json {
            println!("Stopped {}: {}", id.0, view.file_name);
        }
        Ok(ExitCode::SUCCESS)
    } else {
        Err(format!("download {} ended as {:?}, not finished", id.0, view.state).into())
    }
}
