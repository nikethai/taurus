//! `taurus stop <ID>`: ends a live recording and keeps what was recorded.
//!
//! The recording runs in the process that `add`, `resume` or `run` started,
//! and the engine allows one process per data dir. So `stop` asks that
//! process: it leaves a request file, the queue runner there picks it up
//! within a moment and stops the recording (Ctrl-C would leave it queued
//! instead), and `stop` waits for the job to end. With no such process,
//! nothing runs and there is nothing to stop.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use taurus_core::{Engine, EngineConfig, JobId, JobKind, JobState, JobView};

use crate::Result;

/// The folder of request files, one empty file per job id.
fn requests_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("stop-requests")
}

/// How long `stop` waits for the other process: a graceful stop of yt-dlp
/// (10 s), then ffmpeg making the recording playable.
const WAIT: Duration = Duration::from_secs(90);
const POLL: Duration = Duration::from_millis(200);

/// Whether the job is a media job that runs now.
fn recording(view: &JobView) -> bool {
    view.kind == JobKind::Media && matches!(view.state, JobState::Recording | JobState::Downloading)
}

fn find(views: Vec<JobView>, id: JobId) -> Result<JobView> {
    views
        .into_iter()
        .find(|v| v.id == id)
        .ok_or_else(|| format!("download {} not found", id.0).into())
}

fn not_recording(id: JobId) -> Box<dyn std::error::Error> {
    format!("download {} is not recording", id.0).into()
}

/// `stop` with no other process holding the data dir: nothing records.
pub fn local(engine: &Engine, id: JobId) -> Result<ExitCode> {
    find(engine.snapshot(), id)?;
    Err(not_recording(id))
}

/// Asks the process that holds `data_dir` to stop job `id`, and waits for it
/// to end. Prints the finished job (JSON with `--json`).
pub async fn request(data_dir: PathBuf, id: JobId, json: bool) -> Result<ExitCode> {
    let config = EngineConfig::new(data_dir.clone());
    if !recording(&find(Engine::read_snapshot(&config)?, id)?) {
        return Err(not_recording(id));
    }
    let dir = requests_dir(&data_dir);
    std::fs::create_dir_all(&dir)?;
    let file = dir.join(id.0.to_string());
    std::fs::write(&file, b"")?;
    let deadline = tokio::time::Instant::now() + WAIT;
    let view = loop {
        tokio::time::sleep(POLL).await;
        let view = find(Engine::read_snapshot(&config)?, id)?;
        if !recording(&view) {
            break view;
        }
        if tokio::time::Instant::now() >= deadline {
            let _ = std::fs::remove_file(&file);
            return Err(
                format!("download {} did not stop within {} s", id.0, WAIT.as_secs()).into(),
            );
        }
    };
    if json {
        println!("{}", serde_json::to_string(&view)?);
    } else {
        println!("Stopped {}: {}", id.0, view.file_name);
    }
    Ok(if view.state == JobState::Completed {
        ExitCode::SUCCESS
    } else {
        eprintln!("error: download {} ended as {:?}", id.0, view.state);
        ExitCode::FAILURE
    })
}

/// Forgets requests left over from before this process began.
pub fn clear_stale(data_dir: &Path) {
    if let Ok(entries) = std::fs::read_dir(requests_dir(data_dir)) {
        for entry in entries.flatten() {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// The jobs that other processes asked to stop since the last call. Each
/// request is taken (its file removed) once.
pub fn take_requests(data_dir: &Path) -> Vec<JobId> {
    let Ok(entries) = std::fs::read_dir(requests_dir(data_dir)) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let id = entry.file_name().to_str()?.parse::<i64>().ok()?;
            std::fs::remove_file(entry.path()).ok()?;
            Some(JobId(id))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_are_taken_once() {
        let tmp = std::env::temp_dir().join(format!("hyades-stop-{}", std::process::id()));
        let dir = requests_dir(&tmp);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("7"), b"").unwrap();
        std::fs::write(dir.join("not-an-id"), b"").unwrap();
        assert_eq!(take_requests(&tmp), vec![JobId(7)]);
        assert!(take_requests(&tmp).is_empty());
        std::fs::write(dir.join("9"), b"").unwrap();
        clear_stale(&tmp);
        assert!(take_requests(&tmp).is_empty());
        std::fs::remove_dir_all(&tmp).unwrap();
    }
}
