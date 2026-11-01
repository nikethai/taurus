//! Stop requests between processes. The engine owns its data dir
//! exclusively, so another process (`taurus stop`) cannot call
//! `stop_recording` on a running engine; it leaves a file in
//! `<data dir>/stop-requests/` instead. Every open engine polls that folder.
//! Taking a request (deleting its file) is the acknowledgement: the file is
//! gone once an engine has it.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use tokio::sync::mpsc;

use crate::errors::CoreError;
use crate::store::JobId;

use super::actor::Command;
use super::{Engine, EngineConfig, io_error};

const DIR: &str = "stop-requests";
const POLL: Duration = Duration::from_millis(250);
/// A request older than this was left by a `stop` that gave up or died.
const STALE: Duration = Duration::from_secs(120);

fn dir(data_dir: &Path) -> PathBuf {
    data_dir.join(DIR)
}

impl Engine {
    /// Asks the engine that holds `config.data_dir` (in any process, within
    /// about a quarter of a second) to stop media job `id` as
    /// `stop_recording` does. Returns at once, whether or not an engine
    /// runs; the request is answered by its file disappearing
    /// (`stop_request_pending`). Withdraw an unanswered one with
    /// `withdraw_stop_request`.
    pub fn request_stop(config: &EngineConfig, id: JobId) -> Result<(), CoreError> {
        let dir = dir(&config.data_dir);
        std::fs::create_dir_all(&dir).map_err(io_error)?;
        // Written under a name the engine ignores, then renamed into place.
        let temp = dir.join(format!("{}.{}.tmp", id.0, std::process::id()));
        std::fs::write(&temp, b"").map_err(io_error)?;
        std::fs::rename(&temp, dir.join(id.0.to_string())).map_err(io_error)
    }

    /// Whether the request for `id` is still waiting for an engine.
    pub fn stop_request_pending(config: &EngineConfig, id: JobId) -> bool {
        dir(&config.data_dir).join(id.0.to_string()).exists()
    }

    /// Takes back an unanswered request.
    pub fn withdraw_stop_request(config: &EngineConfig, id: JobId) {
        let _ = std::fs::remove_file(dir(&config.data_dir).join(id.0.to_string()));
    }
}

/// Deletes the requests left from before this engine opened, then answers
/// new ones until the engine stops. The task holds only a weak sender, so it
/// does not keep the engine alive.
pub(super) fn watch(data_dir: &Path, commands: &mpsc::Sender<Command>) {
    let dir = dir(data_dir);
    forget_stale(&dir);
    let commands = commands.downgrade();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(POLL);
        loop {
            tick.tick().await;
            // Gone with the last handle, or shut down: requests stay unanswered.
            let Some(commands) = commands.upgrade().filter(|c| !c.is_closed()) else {
                return;
            };
            for id in take(&dir) {
                let (reply, response) = tokio::sync::oneshot::channel();
                if commands
                    .send(Command::StopRecording { id, reply })
                    .await
                    .is_err()
                {
                    return;
                }
                // Not a media job, or gone: nothing to stop, nothing to say.
                if let Ok(Err(e)) = response.await {
                    tracing::debug!(job = id.0, "stop request ignored: {e}");
                }
            }
        }
    });
}

/// The requests in `dir`, each one's file deleted. Names that are not a job
/// id are left alone. Read inline, not on the blocking pool: the folder is
/// tiny, and usually missing.
fn take(dir: &Path) -> Vec<JobId> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut ids = Vec::new();
    for entry in entries.flatten() {
        let Some(id) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<i64>().ok())
        else {
            continue;
        };
        if std::fs::remove_file(entry.path()).is_ok() {
            ids.push(JobId(id));
        }
    }
    ids
}

fn forget_stale(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let age = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| SystemTime::now().duration_since(t).ok());
        if age.is_some_and(|age| age > STALE) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_are_taken_once_and_odd_names_left() {
        let tmp = tempfile::tempdir().unwrap();
        let config = EngineConfig::new(tmp.path().to_path_buf());
        let d = dir(tmp.path());
        Engine::request_stop(&config, JobId(7)).unwrap();
        assert!(Engine::stop_request_pending(&config, JobId(7)));
        std::fs::write(d.join("not-an-id"), b"").unwrap();
        assert_eq!(take(&d), vec![JobId(7)]);
        assert!(!Engine::stop_request_pending(&config, JobId(7)));
        assert!(take(&d).is_empty());
        assert!(d.join("not-an-id").exists());
        Engine::request_stop(&config, JobId(9)).unwrap();
        Engine::withdraw_stop_request(&config, JobId(9));
        assert!(take(&d).is_empty());
    }

    #[test]
    fn old_requests_are_forgotten() {
        let tmp = tempfile::tempdir().unwrap();
        let d = dir(tmp.path());
        std::fs::create_dir_all(&d).unwrap();
        let old = d.join("1");
        let fresh = d.join("2");
        std::fs::write(&old, b"").unwrap();
        std::fs::write(&fresh, b"").unwrap();
        let file = std::fs::File::options().write(true).open(&old).unwrap();
        file.set_modified(SystemTime::now() - Duration::from_secs(600))
            .unwrap();
        forget_stale(&d);
        assert!(!old.exists());
        assert!(fresh.exists());
    }
}
