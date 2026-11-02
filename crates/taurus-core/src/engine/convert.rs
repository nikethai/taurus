//! Convert jobs in the engine: adding them for a finished job's files or a
//! local file, starting their runner (`convert_job`) and deleting what a
//! removed one leaves. Converting never overwrites and never touches the
//! source (see `convert_job`).

use std::path::{Path, PathBuf};

use tokio_util::sync::CancellationToken;

use crate::convert::{ConvertSource, ConvertSpec, output_path};
use crate::errors::{CoreError, ErrorKind};
use crate::filename::PARTIAL_EXT;
use crate::http::JobOutcome;
use crate::media::ConvertPreset;
use crate::store::{JobId, JobKind, JobRecord, JobState, NewJob};

use super::actor::{Actor, Command, JobMsg, Run};
use super::convert_job::{ConvertJob, remove_own, run_convert_job};
use super::jobs::view_of;
use super::{Engine, Event, JobView, io_error, no_such_job, read, write};

/// Extensions of files that are still being written by a download (ours,
/// yt-dlp's, browsers').
const UNFINISHED_EXTS: [&str; 4] = [PARTIAL_EXT, "part", "ytdl", "crdownload"];

impl Engine {
    /// Converts a finished file with ffmpeg, as a queued job (it counts
    /// toward `max_active`, shows its progress and gets ffmpeg first, like a
    /// media job): `Mp3` makes `<stem> (mp3).mp3`, `SmallerMp4` makes
    /// `<stem> (smaller).mp4`, next to the source. The output never
    /// overwrites a file (a name that is taken gets ` (1)`, ` (2)`, ...) and
    /// the source is never modified; a conversion that fails or is stopped
    /// deletes only its own partial output. The job is named after its
    /// output and Completed with that file.
    ///
    /// A source that does not exist, is not a file or is still downloading
    /// is refused, and so is a job that has not completed. A finished
    /// playlist makes one job per finished file: this returns the first
    /// job's id, `convert_many` all of them. A torrent's files are
    /// converted by path.
    pub async fn convert(
        &self,
        source: ConvertSource,
        preset: ConvertPreset,
    ) -> Result<JobId, CoreError> {
        let ids = self.convert_many(source, preset).await?;
        ids.first().copied().ok_or_else(|| CoreError {
            kind: ErrorKind::Unknown,
            detail: "nothing to convert".to_string(),
        })
    }

    /// `convert`, answering the id of the job of each file (in the order of
    /// the playlist), all added or none.
    pub async fn convert_many(
        &self,
        source: ConvertSource,
        preset: ConvertPreset,
    ) -> Result<Vec<JobId>, CoreError> {
        self.check_running()?;
        let files = match source {
            ConvertSource::Job(id) => {
                let view = self.view(id).ok_or_else(no_such_job)?;
                finished_files(&view)?
            }
            ConvertSource::Path(path) => vec![path],
        };
        let mut checked = Vec::with_capacity(files.len());
        for file in files {
            checked.push(self.check_source(&file).await?);
        }
        self.call(|reply| Command::AddConvert {
            files: checked,
            preset,
            reply,
        })
        .await?
    }

    /// The file as the engine will record it (absolute, no symlinks), if it
    /// is a file that no download is still writing.
    async fn check_source(&self, path: &Path) -> Result<PathBuf, CoreError> {
        let missing = |what: &str| CoreError {
            kind: ErrorKind::Unknown,
            detail: format!("{what}: {}", path.display()),
        };
        let is_file = tokio::fs::metadata(path).await.is_ok_and(|m| m.is_file());
        if !is_file {
            return Err(missing("not a file that exists"));
        }
        let path = dunce::canonicalize(path).map_err(io_error)?;
        let unfinished_ext = path
            .extension()
            .is_some_and(|e| UNFINISHED_EXTS.iter().any(|u| e.eq_ignore_ascii_case(u)));
        if unfinished_ext || self.still_downloading(&path) {
            return Err(missing("still downloading"));
        }
        Ok(path)
    }

    /// Whether an unfinished job is writing the file `path`: by its name in
    /// the job's folder, or, for a torrent, anywhere inside the torrent's
    /// own folder (its name), where it writes all its files.
    fn still_downloading(&self, path: &Path) -> bool {
        let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else {
            return false;
        };
        read(&self.views).values().any(|v| {
            let unfinished = v.kind != JobKind::Convert
                && !matches!(v.state, JobState::Completed | JobState::Seeding);
            let writes = match v.kind {
                JobKind::Torrent => {
                    !v.file_name.is_empty() && path.starts_with(v.dest_dir.join(&v.file_name))
                }
                _ => v.dest_dir == dir && name == v.file_name.as_str(),
            };
            unfinished && writes
        })
    }
}

/// The files a finished job holds, for converting: a web download's or a
/// conversion's file, a video's file, a playlist's finished files.
fn finished_files(view: &JobView) -> Result<Vec<PathBuf>, CoreError> {
    let refuse = |detail: &str| CoreError {
        kind: ErrorKind::Unknown,
        detail: detail.to_string(),
    };
    if view.state != JobState::Completed {
        return Err(refuse("the job has not finished downloading"));
    }
    match view.kind {
        JobKind::Http | JobKind::Convert => Ok(vec![view.dest_dir.join(&view.file_name)]),
        JobKind::Media => match &view.media {
            Some(media) if !media.files.is_empty() => Ok(media.files.clone()),
            // A playlist's files are known only to the run that made them.
            _ => Err(refuse(
                "this job's files are not known any more; convert them by path",
            )),
        },
        JobKind::Torrent => Err(refuse("convert a torrent's files by path")),
    }
}

impl Actor {
    /// Queues one convert job per file, named after its output, which is
    /// next to the file. All or none.
    pub(super) fn add_convert(
        &mut self,
        files: Vec<PathBuf>,
        preset: ConvertPreset,
    ) -> Result<Vec<JobId>, CoreError> {
        let mut added: Vec<JobRecord> = Vec::new();
        for file in files {
            match self.insert_convert(&file, preset) {
                Ok(job) => added.push(job),
                Err(e) => {
                    for job in &added {
                        if let Err(cleanup) = self.store.delete_job(job.id) {
                            tracing::warn!(
                                job = job.id.0,
                                "could not remove a half-added job: {cleanup}"
                            );
                        }
                    }
                    return Err(e);
                }
            }
        }
        for job in &added {
            let view = view_of(&self.store, job, 0);
            write(&self.views).insert(job.id, view.clone());
            self.emit(Event::JobAdded(view));
        }
        self.after_change();
        Ok(added.iter().map(|job| job.id).collect())
    }

    /// Stores the job converting `file`, named after its output.
    fn insert_convert(&self, file: &Path, preset: ConvertPreset) -> Result<JobRecord, CoreError> {
        let dir = file.parent().ok_or_else(|| CoreError {
            kind: ErrorKind::Unknown,
            detail: format!("{} has no folder", file.display()),
        })?;
        let reserved = self.store.reserved_names(dir)?;
        let output = output_path(file, preset, &reserved);
        let name = output
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .ok_or_else(no_such_job)?;
        let id = self.store.insert_job(&NewJob {
            url: file.to_string_lossy().into_owned(),
            source_url: file.to_string_lossy().into_owned(),
            dest_dir: dir.to_path_buf(),
            file_name: name,
            connections: 1,
            mirrors: Vec::new(),
            checksum: None,
            referer: None,
            queue_pos: 0,
            kind: JobKind::Convert,
            info_hash: None,
            torrent_meta: None,
            only_files: None,
        })?;
        let spec = ConvertSpec {
            source: file.to_path_buf(),
            preset,
            partial: None,
        };
        let stored = self
            .store
            .set_convert(id, &spec)
            .and_then(|()| self.store.get_job(id)?.ok_or_else(no_such_job));
        stored.inspect_err(|_| {
            if let Err(cleanup) = self.store.delete_job(id) {
                tracing::warn!(job = id.0, "could not remove a half-added job: {cleanup}");
            }
        })
    }

    /// Whether the job is a convert job, as its view says.
    pub(super) fn is_convert(&self, id: JobId) -> bool {
        read(&self.views)
            .get(&id)
            .is_some_and(|v| v.kind == JobKind::Convert)
    }

    /// Spawns the convert job's runner; the job is Downloading from here
    /// on, and holds a `max_active` slot.
    pub(super) fn start_convert(&mut self, id: JobId) {
        let job = match self.store.get_job(id) {
            Ok(Some(job)) if job.convert.is_some() => job,
            Ok(_) => {
                return self.fail(
                    id,
                    CoreError {
                        kind: ErrorKind::Unknown,
                        detail: "the convert job has no source".to_string(),
                    },
                );
            }
            Err(e) => return self.fail(id, e),
        };
        if let Err(e) = self.store.set_state(id, JobState::Downloading) {
            tracing::warn!(job = id.0, "could not record the start: {e}");
        }
        let cancel = CancellationToken::new();
        let ctx = ConvertJob {
            job,
            tools: self.tools.clone(),
            store: self.store.clone(),
            grace: self.timing.media_stop_grace,
            events: self.events.clone(),
            cancel: cancel.clone(),
            msgs: self.job_tx.clone(),
        };
        let tx = self.job_tx.clone();
        tokio::spawn(async move {
            // A task of its own, so a panic still ends in `Finished`; the
            // runner's messages all precede it.
            let outcome = tokio::spawn(run_convert_job(ctx))
                .await
                .unwrap_or_else(|e| {
                    JobOutcome::Failed(CoreError {
                        kind: ErrorKind::Unknown,
                        detail: format!("convert task crashed: {e}"),
                    })
                });
            let _ = tx.send(JobMsg::Finished { id, outcome });
        });
        self.running.insert(
            id,
            Run {
                cancel,
                downloaded_at_start: 0,
            },
        );
        self.set_view_state(id, JobState::Downloading);
    }

    /// Deletes what the removed convert job leaves: the temporary file its
    /// run recorded (always: it is the job's own), and with `delete_files`
    /// the output of a completed job (unless another job has the name or
    /// its size changed). Never the source.
    pub(super) fn delete_convert(&self, job: &JobRecord, delete_files: bool) {
        let spec = job.convert.as_ref();
        if let Some(partial) = spec.and_then(|s| s.partial.as_ref()) {
            remove_own(partial);
        }
        if !delete_files || job.state != JobState::Completed {
            return;
        }
        let output = job.dest_dir.join(&job.file_name);
        let used = self
            .store
            .name_used_by_other(job.id, &job.dest_dir, &job.file_name)
            .unwrap_or(true);
        let source = spec.is_some_and(|s| s.source == output);
        let ours = std::fs::metadata(&output)
            .is_ok_and(|m| m.is_file() && job.total.is_none_or(|total| m.len() == total));
        if ours
            && !used
            && !source
            && let Err(e) = std::fs::remove_file(&output)
        {
            tracing::warn!(job = job.id.0, "could not delete {}: {e}", output.display());
        }
    }
}
