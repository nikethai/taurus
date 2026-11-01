//! A media job's runner: gets the tools, runs yt-dlp (once more after a
//! forced yt-dlp update when the site's extractor broke), and records how
//! it ended. It reports to the actor through `JobMsg`s.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;

use crate::errors::{CoreError, ErrorKind};
use crate::http::JobOutcome;
use crate::media::run::{
    Ended, MediaPhase, Stop, Tracker, download_args, recorded_part, remux_part, run_process,
};
use crate::media::{YtDlp, classify_stderr_with, failure_detail, standard_quality};
use crate::plan::Segment;
use crate::store::{JobRecord, JobState, Store};
use crate::system::marks::mark_downloaded;
use crate::tools::{ToolKind, ToolManager, ToolState, ToolStatus};

use super::Event;
use super::actor::JobMsg;
use super::media::MediaUpdate;

/// What a media job's runner needs.
pub(super) struct MediaJob {
    pub(super) job: JobRecord,
    pub(super) tools: Arc<ToolManager>,
    pub(super) store: Store,
    /// How long yt-dlp may take to stop after the interrupt.
    pub(super) grace: Duration,
    /// For `Event::ToolStatus` while the tools are installed.
    pub(super) events: broadcast::Sender<Event>,
    /// Pause, removal or shutdown.
    pub(super) cancel: CancellationToken,
    /// `stop_recording`.
    pub(super) finish: CancellationToken,
    pub(super) msgs: mpsc::UnboundedSender<JobMsg>,
}

/// Runs the job and records how it ended in the store (Completed with the
/// finished file's name, Paused, or Failed with its error), as `run_job`
/// does for a web download.
pub(super) async fn run_media_job(ctx: MediaJob) -> JobOutcome {
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
        tracing::warn!(job = id.0, "could not record how the media job ended: {e}");
    }
    outcome
}

async fn drive(ctx: &MediaJob) -> Result<JobOutcome, CoreError> {
    let job = &ctx.job;
    let options = job.media.as_ref().ok_or_else(|| CoreError {
        kind: ErrorKind::Unknown,
        detail: "the media job has no options".to_string(),
    })?;
    let ffmpeg = tokio::select! {
        ffmpeg = get_tools(ctx) => ffmpeg?,
        () = ctx.cancel.cancelled() => return Ok(JobOutcome::Paused),
        () = ctx.finish.cancelled() => return Ok(JobOutcome::Paused),
    };
    let custom_quality = standard_quality(&options.quality).is_none();
    let first_phase = if options.live_from_start {
        MediaPhase::WaitingForStream
    } else {
        MediaPhase::Downloading
    };
    let mut retried = false;
    loop {
        let yt_dlp = YtDlp::prepare(&ctx.tools).await?;
        let mut command = yt_dlp.command();
        command
            .args(download_args(options, &job.dest_dir, &job.url))
            .current_dir(&job.dest_dir);
        let mut tracker = Tracker::new(&job.dest_dir, first_phase);
        let started = SystemTime::now();
        let mut live_since = None;
        ctx.update(&tracker, None, true);
        let ended = run_process(command, &ctx.cancel, &ctx.finish, ctx.grace, |line| {
            let change = tracker.apply(line);
            if let Some((downloaded, total, speed_bps)) = change.progress {
                let _ = ctx.msgs.send(JobMsg::Progress {
                    id: job.id,
                    downloaded,
                    total,
                    speed_bps,
                });
            }
            let elapsed = options.live_from_start.then(|| {
                tracker.progressed.then(|| {
                    live_since
                        .get_or_insert_with(Instant::now)
                        .elapsed()
                        .as_secs()
                })
            });
            let elapsed = elapsed.flatten();
            if change.view || (elapsed.is_some() && change.progress.is_some()) {
                ctx.update(&tracker, elapsed, change.view);
            }
        })
        .await?;
        match ended.stopped {
            Some(Stop::Pause) => return Ok(JobOutcome::Paused),
            Some(Stop::Finish) => {
                return finish_recording(ctx, &mut tracker, started, &ffmpeg).await;
            }
            None if ended.success() => {
                tracker.finish();
                ctx.update(&tracker, None, true);
                return complete(ctx, &tracker);
            }
            None => {
                let (kind, broke) = classify_stderr_with(&ended.stderr, custom_quality);
                if broke && !retried {
                    retried = true;
                    if update_after_break(ctx, &ended).await {
                        continue;
                    }
                }
                return Err(CoreError {
                    kind,
                    detail: failure_detail(&ended.stderr),
                });
            }
        }
    }
}

/// Installs whatever tool is missing (phase GettingTools, each step sent as
/// `Event::ToolStatus`), then checks for a newer yt-dlp once a day. Returns
/// ffmpeg's path.
async fn get_tools(ctx: &MediaJob) -> Result<PathBuf, CoreError> {
    let emit = emitter(&ctx.events);
    let missing = ctx
        .tools
        .status()
        .iter()
        .any(|s| s.state != ToolState::Ready);
    if missing {
        let tracker = Tracker::new(&ctx.job.dest_dir, MediaPhase::GettingTools);
        ctx.update(&tracker, None, true);
    }
    let (yt_dlp, ffmpeg, deno) = tokio::join!(
        ctx.tools.ensure(ToolKind::YtDlp, &emit),
        ctx.tools.ensure(ToolKind::Ffmpeg, &emit),
        ctx.tools.ensure(ToolKind::Deno, &emit),
    );
    yt_dlp?;
    deno?;
    let ffmpeg = ffmpeg?;
    // At most once per 24 h; a failed check keeps the installed yt-dlp.
    if let Err(e) = ctx.tools.update(ToolKind::YtDlp, false, &emit).await {
        tracing::info!("could not check for a newer yt-dlp: {e}");
    }
    Ok(ffmpeg)
}

/// yt-dlp's extractor for the site broke: updates yt-dlp at once. True if
/// a newer version is now installed (worth one more run).
async fn update_after_break(ctx: &MediaJob, ended: &Ended) -> bool {
    let id = ctx.job.id;
    tracing::info!(
        job = id.0,
        "yt-dlp could not read the site ({}); updating it",
        failure_detail(&ended.stderr)
    );
    let emit = emitter(&ctx.events);
    match ctx.tools.update(ToolKind::YtDlp, true, &emit).await {
        Ok(true) => true,
        Ok(false) => {
            tracing::info!(job = id.0, "no newer yt-dlp is published");
            false
        }
        Err(e) => {
            tracing::info!(job = id.0, "could not update yt-dlp: {e}");
            false
        }
    }
}

/// The recording was stopped: keeps what yt-dlp finished, else makes its
/// partial file playable with ffmpeg.
async fn finish_recording(
    ctx: &MediaJob,
    tracker: &mut Tracker,
    started: SystemTime,
    ffmpeg: &Path,
) -> Result<JobOutcome, CoreError> {
    tracker.files.retain(|f| f.is_file());
    if tracker.files.is_empty() {
        let part =
            recorded_part(&ctx.job.dest_dir, &tracker.parts, started).ok_or_else(|| CoreError {
                kind: ErrorKind::Unknown,
                detail: "the recording was stopped before anything was recorded".to_string(),
            })?;
        let file = remux_part(ffmpeg, &part).await?;
        tracker.files.push(file);
    }
    tracker.finish();
    ctx.update(tracker, None, true);
    complete(ctx, tracker)
}

/// Records the job as Completed: named after its file (a playlist after
/// its folder), sized by its files, which are marked as downloaded.
fn complete(ctx: &MediaJob, tracker: &Tracker) -> Result<JobOutcome, CoreError> {
    let mut job = ctx
        .store
        .get_job(ctx.job.id)?
        .unwrap_or_else(|| ctx.job.clone());
    let files: Vec<&PathBuf> = tracker.files.iter().filter(|f| f.is_file()).collect();
    let mut total = 0u64;
    for file in &files {
        mark_downloaded(file, &job.source_url, job.referer.as_deref());
        total += std::fs::metadata(file).map_or(0, |m| m.len());
    }
    let playlist = job.media.as_ref().is_some_and(|m| m.playlist);
    if let Some(name) = files
        .last()
        .and_then(|f| name_of(&job.dest_dir, f, playlist))
    {
        job.file_name = name;
    }
    job.total = Some(total);
    job.state = JobState::Completed;
    job.error = None;
    ctx.store.update_job(&job)?;
    ctx.store.replace_segments(
        job.id,
        &[Segment {
            start: 0,
            end: total,
            done: total,
        }],
    )?;
    let _ = ctx.msgs.send(JobMsg::Progress {
        id: job.id,
        downloaded: total,
        total: Some(total),
        speed_bps: 0,
    });
    let path = files
        .last()
        .map_or_else(|| job.dest_dir.join(&job.file_name), |f| (*f).clone());
    Ok(JobOutcome::Completed(path))
}

/// The job's name for a finished `file`: the file's own name, or for a
/// playlist the folder it is in, directly inside `dest_dir`.
fn name_of(dest_dir: &Path, file: &Path, playlist: bool) -> Option<String> {
    if playlist
        && let Ok(inside) = file.strip_prefix(dest_dir)
        && inside.components().count() > 1
    {
        let folder = inside.components().next()?;
        return Some(folder.as_os_str().to_string_lossy().into_owned());
    }
    file.file_name().map(|n| n.to_string_lossy().into_owned())
}

impl MediaJob {
    /// Sends the tracker's state as the job's media view.
    fn update(&self, tracker: &Tracker, live_elapsed_secs: Option<u64>, announce: bool) {
        let _ = self.msgs.send(JobMsg::Media {
            id: self.job.id,
            update: MediaUpdate {
                phase: tracker.phase,
                entries_done: tracker.entries_done,
                entries_total: tracker.entries_total,
                files: tracker.files.clone(),
                live_elapsed_secs,
                announce,
            },
        });
    }
}

fn emitter(events: &broadcast::Sender<Event>) -> impl Fn(ToolStatus) + Send + Sync + use<> {
    let events = events.clone();
    move |status| {
        // No subscriber is fine.
        let _ = events.send(Event::ToolStatus(status));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_of_finished_files() {
        let dest = Path::new("/d");
        let single = Path::new("/d/A Video [x].mp4");
        assert_eq!(name_of(dest, single, false).unwrap(), "A Video [x].mp4");
        let entry = Path::new("/d/My List/001 - A [x].mp4");
        assert_eq!(name_of(dest, entry, true).unwrap(), "My List");
        // A playlist file outside the folder keeps its own name.
        assert_eq!(name_of(dest, Path::new("/e/b.mp4"), true).unwrap(), "b.mp4");
        assert_eq!(name_of(dest, Path::new("/d/b.mp4"), true).unwrap(), "b.mp4");
    }
}
