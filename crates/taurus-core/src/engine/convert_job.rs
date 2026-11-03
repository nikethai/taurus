//! A convert job's runner: gets ffmpeg, reads the file's length with
//! ffprobe, runs ffmpeg into a private temporary file, and only then moves
//! the result to its final name. It reports to the actor through `JobMsg`s.
//!
//! Never overwriting, and never touching a file that is not the job's:
//!
//! * ffmpeg always runs with `-n`, and writes only to a temporary file,
//!   `out.<ext>` inside a folder `.hyades-convert-<job>-<n>` next to the
//!   source that the run itself created (`create_dir` refuses any name that
//!   exists, so no one else was given it). The temporary file is recorded in
//!   the job (`ConvertSpec::partial`) only once that folder exists and
//!   before ffmpeg starts, so the job only ever deletes a file in a folder
//!   of its own, and then that folder if it is empty: when the run fails or
//!   is stopped, when a run is interrupted by a restart, or when the job is
//!   removed.
//! * The final name is created exclusively (`create_new`) just before the
//!   temporary file is renamed onto that empty file of ours (tried again
//!   while Windows briefly locks the new file); a name another process took
//!   meanwhile is simply not used (the next free one is).
//!   So the final name is never written but through a file that did not
//!   exist a moment before, however the names of other files change.
//! * The source is only ever read.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::process::Command;
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;

use crate::convert::probe::{probe_duration, progress_ms};
use crate::convert::{ConvertSpec, ffmpeg_args, output_path};
use crate::errors::{CoreError, ErrorKind, classify_io};
use crate::http::{JobOutcome, rename_with_retry};
use crate::media::run::{Output, run_process};
use crate::plan::Segment;
use crate::store::{JobId, JobRecord, JobState, Store};
use crate::tools::{ToolKind, ToolManager};

use super::Event;
use super::actor::JobMsg;
use super::media_job::emitter;

/// Names tried for the final file, or the temporary folder, before giving
/// up (each is taken by another process between choosing it and creating
/// it).
const NAME_ATTEMPTS: usize = 100;
/// Start of the name of a temporary folder of a convert job.
const TEMP_PREFIX: &str = ".hyades-convert-";
/// The stem of the temporary file inside it.
const TEMP_STEM: &str = "out";

/// What a convert job's runner needs.
pub(super) struct ConvertJob {
    pub(super) job: JobRecord,
    pub(super) tools: Arc<ToolManager>,
    pub(super) store: Store,
    /// How long ffmpeg may take to stop after the interrupt.
    pub(super) grace: Duration,
    /// For `Event::ToolStatus` while ffmpeg is installed.
    pub(super) events: broadcast::Sender<Event>,
    /// Pause, removal or shutdown.
    pub(super) cancel: CancellationToken,
    pub(super) msgs: mpsc::UnboundedSender<JobMsg>,
}

/// Runs the job and records how it ended in the store (Completed with the
/// output's name, Paused, or Failed with its error).
pub(super) async fn run_convert_job(ctx: ConvertJob) -> JobOutcome {
    let outcome = match drive(&ctx).await {
        Ok(outcome) => outcome,
        Err(e) => JobOutcome::Failed(e),
    };
    let id = ctx.job.id;
    let recorded = match &outcome {
        JobOutcome::Completed(_) => Ok(()),
        JobOutcome::Paused => ctx.store.set_state(id, JobState::Paused),
        JobOutcome::Failed(e) => ctx.store.get_job(id).and_then(|job| match job {
            Some(mut job) => {
                job.state = JobState::Failed;
                job.error = Some((e.kind, e.detail.clone()));
                ctx.store.update_job(&job)
            }
            None => Ok(()),
        }),
    };
    if let Err(e) = recorded {
        tracing::warn!(
            job = id.0,
            "could not record how the convert job ended: {e}"
        );
    }
    outcome
}

async fn drive(ctx: &ConvertJob) -> Result<JobOutcome, CoreError> {
    let id = ctx.job.id;
    let spec = ctx.job.convert.clone().ok_or_else(|| CoreError {
        kind: ErrorKind::Unknown,
        detail: "the convert job has no source".to_string(),
    })?;
    let emit = emitter(&ctx.events);
    let ffmpeg = tokio::select! {
        ffmpeg = ctx.tools.ensure(ToolKind::Ffmpeg, &emit) => ffmpeg?,
        () = ctx.cancel.cancelled() => return Ok(JobOutcome::Paused),
    };
    if !spec.source.is_file() {
        return Err(CoreError {
            kind: ErrorKind::Unknown,
            detail: format!("the file to convert is gone: {}", spec.source.display()),
        });
    }
    // What an earlier run (killed by a crash) left: recorded as the job's
    // own before ffmpeg started.
    if let Some(old) = &spec.partial {
        remove_own(old);
    }
    let ffprobe = ffmpeg.with_file_name(format!("ffprobe{}", std::env::consts::EXE_SUFFIX));
    let total_ms = tokio::select! {
        length = probe_duration(&ffprobe, &spec.source) => length.map(millis),
        () = ctx.cancel.cancelled() => return Ok(JobOutcome::Paused),
    };
    let temp = claim_temp(
        &ctx.job.dest_dir,
        id,
        crate::convert::extension(spec.preset),
    )
    .map_err(|e| io_error(&e))?;
    let mut recorded = ConvertSpec {
        partial: Some(temp.clone()),
        ..spec.clone()
    };
    if let Err(e) = ctx.store.set_convert(id, &recorded) {
        remove_own(&temp);
        return Err(e);
    }
    let result = convert(ctx, &ffmpeg, &spec, &temp, total_ms).await;
    // Whatever happened but a published output, the temporary file is
    // ours to delete.
    if !matches!(result, Ok(Some(_))) {
        remove_own(&temp);
    }
    recorded.partial = None;
    if let Err(e) = ctx.store.set_convert(id, &recorded) {
        tracing::warn!(job = id.0, "could not forget the temporary file: {e}");
    }
    match result? {
        Some(path) => complete(ctx, &path),
        None => Ok(JobOutcome::Paused),
    }
}

/// Runs ffmpeg into `temp` and publishes it; `None` if the run was stopped.
async fn convert(
    ctx: &ConvertJob,
    ffmpeg: &Path,
    spec: &ConvertSpec,
    temp: &Path,
    total_ms: Option<u64>,
) -> Result<Option<PathBuf>, CoreError> {
    let mut command = Command::new(ffmpeg);
    command
        .args(ffmpeg_args(spec.preset, &spec.source, temp))
        .stdin(std::process::Stdio::null())
        .current_dir(&ctx.job.dest_dir);
    // Only a pause (or removal, or shutdown) stops a conversion.
    let never = CancellationToken::new();
    let ended = run_process(command, &ctx.cancel, &never, ctx.grace, |out| {
        if let Output::Line(line) = out
            && let Some(ms) = progress_ms(line)
        {
            let done = total_ms.map_or(ms, |total| ms.min(total));
            ctx.progress(done, total_ms);
        }
    })
    .await?;
    if ended.stopped.is_some() {
        return Ok(None);
    }
    if !ended.success() {
        return Err(failure(&ended.stderr, ended.status));
    }
    publish(ctx, spec, temp).await.map(Some)
}

/// Moves the finished temporary file to the output's name, which is first
/// created exclusively (so a file that appeared since the name was chosen
/// is never replaced; the next free name is used instead). Returns the
/// final path.
async fn publish(ctx: &ConvertJob, spec: &ConvertSpec, temp: &Path) -> Result<PathBuf, CoreError> {
    let produced = fs::metadata(temp).is_ok_and(|m| m.is_file() && m.len() > 0);
    if !produced {
        return Err(CoreError {
            kind: ErrorKind::ToolFailed,
            detail: "ffmpeg finished without writing anything".to_string(),
        });
    }
    let dir = &ctx.job.dest_dir;
    for _ in 0..NAME_ATTEMPTS {
        // The names other jobs hold, not the job's own.
        let mut reserved = ctx.store.reserved_names(dir)?;
        reserved.remove(&ctx.job.file_name);
        let target = output_path(&spec.source, spec.preset, &reserved);
        let claimed = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&target);
        match claimed {
            Ok(file) => {
                drop(file);
                // Onto our own empty file, which nobody else was given.
                return match rename_with_retry(temp, &target).await {
                    Ok(()) => {
                        remove_own_folder(temp);
                        Ok(target)
                    }
                    Err(e) => {
                        remove_own(&target);
                        Err(e)
                    }
                };
            }
            // Taken since: the next name.
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(io_error(&e)),
        }
    }
    Err(CoreError {
        kind: ErrorKind::Unknown,
        detail: "no free name for the converted file".to_string(),
    })
}

/// Records the job as Completed with the output `path`, which is now the
/// job's name and size.
fn complete(ctx: &ConvertJob, path: &Path) -> Result<JobOutcome, CoreError> {
    let size = fs::metadata(path).map_or(0, |m| m.len());
    let mut job = ctx
        .store
        .get_job(ctx.job.id)?
        .unwrap_or_else(|| ctx.job.clone());
    if let Some(name) = path.file_name() {
        job.file_name = name.to_string_lossy().into_owned();
    }
    job.total = Some(size);
    job.state = JobState::Completed;
    job.error = None;
    ctx.store.update_job(&job)?;
    ctx.store.replace_segments(
        job.id,
        &[Segment {
            start: 0,
            end: size,
            done: size,
        }],
    )?;
    let _ = ctx.msgs.send(JobMsg::Progress {
        id: job.id,
        downloaded: size,
        total: Some(size),
        speed_bps: 0,
    });
    Ok(JobOutcome::Completed(path.to_path_buf()))
}

/// The error of an ffmpeg that failed: the last line it printed.
fn failure(stderr: &str, status: Option<std::process::ExitStatus>) -> CoreError {
    let line = stderr
        .lines()
        .map(str::trim)
        .rfind(|l| !l.is_empty())
        .unwrap_or("no message");
    let kind = if stderr.contains("No space left on device") {
        ErrorKind::DiskFull
    } else if stderr.contains("Permission denied") {
        ErrorKind::NoPermission
    } else {
        ErrorKind::ToolFailed
    };
    let how = status.map_or_else(|| "was killed".to_string(), |s| format!("failed ({s})"));
    CoreError {
        kind,
        detail: format!("ffmpeg {how}: {line}"),
    }
}

/// The job's temporary file: `out.<ext>` in a folder
/// `.hyades-convert-<job>-<n>` in `dir` that this call created
/// (`create_dir` fails on a name that exists), so the folder and what is in
/// it are the job's own. The file itself is left for ffmpeg to create.
fn claim_temp(dir: &Path, id: JobId, ext: &str) -> io::Result<PathBuf> {
    for n in 0..NAME_ATTEMPTS {
        let folder = dir.join(format!("{TEMP_PREFIX}{}-{n}", id.0));
        match fs::create_dir(&folder) {
            Ok(()) => return Ok(folder.join(format!("{TEMP_STEM}.{ext}"))),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "no free name for the temporary folder of the conversion",
    ))
}

/// Deletes a file the job created (its temporary or placeholder file), and
/// the temporary file's own folder (see `claim_temp`) once empty.
pub(super) fn remove_own(path: &Path) {
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => tracing::warn!("could not delete {}: {e}", path.display()),
    }
    remove_own_folder(path);
}

/// Deletes the folder of the temporary file `temp`, if `temp` is one as
/// `claim_temp` names it and the folder is empty; never any other folder
/// (a placeholder's, or that of a temporary file an earlier version kept
/// next to the source).
fn remove_own_folder(temp: &Path) {
    let is_temp = temp.file_stem().is_some_and(|stem| stem == TEMP_STEM);
    let Some(folder) = temp.parent().filter(|folder| {
        is_temp
            && folder
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(TEMP_PREFIX))
    }) else {
        return;
    };
    match fs::remove_dir(folder) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => tracing::warn!("could not delete {}: {e}", folder.display()),
    }
}

fn millis(length: Duration) -> u64 {
    u64::try_from(length.as_millis()).unwrap_or(u64::MAX)
}

fn io_error(e: &io::Error) -> CoreError {
    CoreError {
        kind: classify_io(e),
        detail: e.to_string(),
    }
}

impl ConvertJob {
    /// Sends the job's progress: milliseconds of the file converted, of
    /// `total_ms` when ffprobe told the length.
    fn progress(&self, done: u64, total_ms: Option<u64>) {
        let _ = self.msgs.send(JobMsg::Progress {
            id: self.job.id,
            downloaded: done,
            total: total_ms,
            speed_bps: 0,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The temporary file lives in a folder the call itself created: a
    /// folder of that name made by anyone else is never used, and so never
    /// emptied or deleted.
    #[test]
    fn the_temporary_file_is_in_a_folder_of_the_jobs_own() {
        let dir = tempfile::tempdir().unwrap();
        let foreign = dir.path().join(".hyades-convert-4-0");
        fs::create_dir(&foreign).unwrap();
        fs::write(foreign.join("out.mp3"), b"not ours").unwrap();

        let temp = claim_temp(dir.path(), JobId(4), "mp3").unwrap();
        assert_eq!(temp, dir.path().join(".hyades-convert-4-1").join("out.mp3"));
        assert!(temp.parent().unwrap().is_dir());
        assert!(!temp.exists(), "ffmpeg (with -n) creates the file itself");

        fs::write(&temp, b"ours").unwrap();
        remove_own(&temp);
        assert!(!temp.parent().unwrap().exists());
        assert_eq!(fs::read(foreign.join("out.mp3")).unwrap(), b"not ours");
    }

    /// Deleting the job's own files never removes a folder that is not its
    /// temporary one: not the destination of a placeholder, nor the folder
    /// of a temporary file recorded by an earlier version (a plain file
    /// next to the source).
    #[test]
    fn only_the_private_folder_is_removed() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join(".hyades-convert-like-name");
        fs::create_dir(&dest).unwrap();
        let placeholder = dest.join("song (mp3).mp3");
        fs::write(&placeholder, b"").unwrap();
        remove_own(&placeholder);
        assert!(dest.is_dir());
        let old = dest.join(".hyades-convert-1-0.mp3");
        fs::write(&old, b"x").unwrap();
        remove_own(&old);
        assert!(dest.is_dir());
        assert!(!old.exists());
    }
}
