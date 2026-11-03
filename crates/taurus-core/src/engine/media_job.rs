//! A media job's runner: gets the tools, runs yt-dlp (once more after a
//! forced yt-dlp update when the site's extractor broke), and records how
//! it ended. It reports to the actor through `JobMsg`s.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use tokio::sync::{broadcast, mpsc, watch};
use tokio_util::sync::CancellationToken;

use crate::errors::{CoreError, ErrorKind};
use crate::http::JobOutcome;
use crate::media::remux::{RemuxPlan, recorded_parts, remux_parts, unmoved_recording};
use crate::media::run::{Ended, MediaPhase, Output, Stop, Tracker, download_args};
use crate::media::{
    YtDlp, classify_stderr_with, failure_detail, live_from_start_refused, only_thumbnail_refused,
    standard_quality,
};
use crate::network::ProxySetting;
use crate::plan::Segment;
use crate::store::{JobRecord, JobState, Store};
use crate::system::marks::mark_downloaded;
use crate::tools::{ToolKind, ToolManager, ToolState, ToolStatus};

use super::Event;
use super::actor::JobMsg;
use super::media::MediaUpdate;
use super::media_rate::run_restartable;

/// What a media job's runner needs.
pub(super) struct MediaJob {
    pub(super) job: JobRecord,
    pub(super) tools: Arc<ToolManager>,
    pub(super) store: Store,
    /// How long yt-dlp may take to stop after the interrupt.
    pub(super) grace: Duration,
    /// The job's share of the speed limit (bytes per second; `None`
    /// without a limit) for its next yt-dlp run. A change while one runs
    /// restarts it (see `media_rate`); a recording's never changes.
    pub(super) rate: watch::Receiver<Option<u64>>,
    /// The proxy setting as the job started: each yt-dlp run is given it.
    pub(super) proxy: ProxySetting,
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
/// does for a web download. A job stopped (`finish`) before anything was
/// recorded is Paused without an error.
pub(super) async fn run_media_job(ctx: MediaJob) -> JobOutcome {
    let outcome = match drive(&ctx).await {
        Ok(outcome) => outcome,
        Err(e) => JobOutcome::Failed(e),
    };
    let id = ctx.job.id;
    let recorded = match &outcome {
        JobOutcome::Completed(_) => Ok(()),
        JobOutcome::Paused if ctx.finish.is_cancelled() => {
            ctx.store.get_job(id).and_then(|job| match job {
                Some(mut job) => {
                    job.state = JobState::Paused;
                    job.error = None;
                    ctx.store.update_job(&job)
                }
                None => Ok(()),
            })
        }
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
    let job_since = added_at(ctx);
    let mut retried = false;
    // From the stream's start, until yt-dlp says the site cannot do that.
    let mut from_start = options.live_from_start;
    // What the job's runs wrote and did not finish, kept across the runs.
    let mut partials = options.partial_files.clone();
    // What a run that was restarted had finished, for the next run's view.
    let mut carried: Option<(Vec<PathBuf>, u32, u32)> = None;
    loop {
        // Not behind another job's yt-dlp update when paused meanwhile.
        let yt_dlp = tokio::select! {
            yt_dlp = YtDlp::prepare(&ctx.tools, &ctx.proxy) => yt_dlp?,
            () = ctx.cancel.cancelled() => return Ok(JobOutcome::Paused),
        };
        let mut rate = ctx.rate.clone();
        let limit_rate = *rate.borrow_and_update();
        let mut command = yt_dlp.command();
        command
            .args(download_args(
                options,
                &job.dest_dir,
                &job.url,
                limit_rate,
                from_start,
            ))
            .current_dir(&job.dest_dir);
        let mut tracker = Tracker::new(&job.dest_dir, first_phase);
        tracker.partials = std::mem::take(&mut partials);
        if let Some((files, done, total)) = carried.take() {
            tracker.files = files;
            tracker.entries_done = done;
            tracker.entries_total = total;
        }
        let started = SystemTime::now();
        let mut live = Live::default();
        ctx.update(&tracker, None, true);
        let (ended, restart) = run_restartable(
            command,
            &ctx.cancel,
            &ctx.finish,
            ctx.grace,
            rate,
            limit_rate,
            |out| {
                let change = match out {
                    Output::Line(line) => tracker.apply(line),
                    Output::Tick => {
                        if options.live_from_start {
                            live.tick(ctx, &tracker);
                        }
                        return;
                    }
                };
                if change.partials {
                    ctx.store_partials(&tracker);
                }
                if let Some((downloaded, total, speed_bps)) = change.progress {
                    ctx.progress(downloaded, total, speed_bps);
                }
                let elapsed = live.elapsed(options.live_from_start, &tracker);
                if change.view || (elapsed.is_some() && change.progress.is_some()) {
                    ctx.update(&tracker, elapsed, change.view);
                }
            },
        )
        .await?;
        partials.clone_from(&tracker.partials);
        // The speed limit changed a lot: yt-dlp, stopped as for a pause,
        // goes on from its partial file at the new rate (no failure, no
        // retry, no state change).
        if restart && ended.stopped == Some(Stop::Pause) {
            tracing::info!(job = job.id.0, "the speed limit changed; restarting yt-dlp");
            carried = Some((tracker.files, tracker.entries_done, tracker.entries_total));
            continue;
        }
        match ended.stopped {
            Some(Stop::Pause) => return Ok(JobOutcome::Paused),
            Some(Stop::Finish) => {
                let since = Since {
                    run: started,
                    job: job_since,
                };
                return finish_recording(ctx, &mut tracker, since, &ffmpeg).await;
            }
            None if ended.success() => {
                tracker.finish();
                ctx.update(&tracker, None, true);
                return complete(ctx, &tracker);
            }
            // Downloaded, but the container cannot hold the thumbnail.
            None if ended.status.is_some() && only_thumbnail_refused(&ended.stderr) => {
                tracker.keep_without_thumbnails(job_since);
                if !tracker.files.is_empty() {
                    ctx.store_partials(&tracker);
                    tracker.finish();
                    ctx.update(&tracker, None, true);
                    return complete(ctx, &tracker);
                }
            }
            // The site has no recording from the start: from now on.
            None if from_start && live_from_start_refused(&ended.stderr) => {
                tracing::info!(
                    job = job.id.0,
                    "the site cannot record this stream from its start; recording from now"
                );
                from_start = false;
                continue;
            }
            None => {}
        }
        let (kind, broke) = classify_stderr_with(&ended.stderr, custom_quality);
        if broke && !retried {
            retried = true;
            tokio::select! {
                newer = update_after_break(ctx, &ended, yt_dlp.path()) => {
                    if newer {
                        continue;
                    }
                }
                () = ctx.cancel.cancelled() => return Ok(JobOutcome::Paused),
                // Nothing was recorded: stopping ends it cleanly.
                () = ctx.finish.cancelled() => return Ok(JobOutcome::Paused),
            }
        }
        return Err(CoreError {
            kind,
            detail: failure_detail(&ended.stderr),
        });
    }
}

/// A live recording's clock and size while it runs: yt-dlp records most
/// live streams through ffmpeg, which prints no progress at all, so the
/// elapsed time runs from the moment it began (`Tracker::started`) and,
/// without progress lines, the size is that of the partial file it writes.
#[derive(Default)]
struct Live {
    since: Option<Instant>,
    /// The partial file's size at the last tick, and when that was.
    last: Option<(u64, Instant)>,
}

impl Live {
    /// Seconds recorded, once a live stream has begun.
    fn elapsed(&mut self, live: bool, tracker: &Tracker) -> Option<u64> {
        if !live || !tracker.started {
            return None;
        }
        Some(
            self.since
                .get_or_insert_with(Instant::now)
                .elapsed()
                .as_secs(),
        )
    }

    /// Another second of a live recording: its elapsed time, and its size
    /// read from the partial files when yt-dlp reports none.
    fn tick(&mut self, ctx: &MediaJob, tracker: &Tracker) {
        let Some(elapsed) = self.elapsed(true, tracker) else {
            return;
        };
        if !tracker.progressed {
            let size: u64 = tracker
                .parts
                .iter()
                .filter_map(|p| std::fs::metadata(p).ok())
                .map(|m| m.len())
                .sum();
            let now = Instant::now();
            let speed = self.last.map_or(0, |(before, at)| {
                let secs = now.duration_since(at).as_secs_f64();
                if secs > 0.0 {
                    (size.saturating_sub(before) as f64 / secs) as u64
                } else {
                    0
                }
            });
            self.last = Some((size, now));
            ctx.progress(size, None, speed);
        }
        ctx.update(tracker, Some(elapsed), false);
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
/// the installed yt-dlp is now another than `used`, the one that failed
/// (worth one more run), whoever installed it: when a site breaks, every
/// job on it breaks, and only the first one's update finds something new.
async fn update_after_break(ctx: &MediaJob, ended: &Ended, used: &Path) -> bool {
    let id = ctx.job.id;
    tracing::info!(
        job = id.0,
        "yt-dlp could not read the site ({}); updating it",
        failure_detail(&ended.stderr)
    );
    let emit = emitter(&ctx.events);
    if let Err(e) = ctx.tools.update(ToolKind::YtDlp, true, &emit).await {
        tracing::info!(job = id.0, "could not update yt-dlp: {e}");
    }
    let installed = ctx
        .tools
        .status()
        .into_iter()
        .find(|s| s.kind == ToolKind::YtDlp)
        .and_then(|s| s.path);
    let other = installed.is_some_and(|path| path != used);
    if !other {
        tracing::info!(job = id.0, "no newer yt-dlp is published");
    }
    other
}

/// When the run started, and when the job was added.
#[derive(Clone, Copy)]
struct Since {
    run: SystemTime,
    job: SystemTime,
}

/// When the job was added (the run's start if the store cannot say).
fn added_at(ctx: &MediaJob) -> SystemTime {
    match ctx.store.created_at(ctx.job.id) {
        Ok(Some(secs)) => {
            SystemTime::UNIX_EPOCH + Duration::from_secs(u64::try_from(secs).unwrap_or(0))
        }
        _ => SystemTime::now(),
    }
}

/// The recording was stopped: keeps what yt-dlp finished, else joins its
/// partial files (video and audio) into one playable file with ffmpeg;
/// if yt-dlp was killed while finalizing it, the file it had renamed but
/// not reported is made playable instead (see `unmoved_recording`).
/// Nothing recorded yet (the stream had not begun) is Paused: nothing to
/// keep, and nothing went wrong.
async fn finish_recording(
    ctx: &MediaJob,
    tracker: &mut Tracker,
    since: Since,
    ffmpeg: &Path,
) -> Result<JobOutcome, CoreError> {
    tracker.files.retain(|f| f.is_file());
    if tracker.files.is_empty() {
        let mut parts = recorded_parts(&ctx.job.dest_dir, tracker, since.run);
        // Nothing of its own (at most another job's, found by name).
        if (parts.0.is_empty() || !parts.1)
            && let Some(part) = unmoved_recording(tracker, since.run)
        {
            parts = (vec![part], true);
        }
        let Some(plan) = RemuxPlan::new(parts) else {
            return Ok(JobOutcome::Paused);
        };
        // A long recording takes a while: pausing, removing or shutting
        // down stops ffmpeg (dropping it kills it) and keeps the parts.
        let file = tokio::select! {
            file = remux_parts(ffmpeg, &plan) => file?,
            () = ctx.cancel.cancelled() => {
                let _ = std::fs::remove_file(&plan.out);
                return Ok(JobOutcome::Paused);
            }
        };
        tracker.files.push(file);
        // yt-dlp did not finish it: its thumbnail and temporary file
        // would be left.
        tracker.remove_leftovers(since.job);
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
    // Compared by components, without a `\\?\` prefix on either side.
    let (dest_dir, file) = (dunce::simplified(dest_dir), dunce::simplified(file));
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
    /// Sends the job's progress (`downloaded` bytes of `total`).
    fn progress(&self, downloaded: u64, total: Option<u64>, speed_bps: u64) {
        let _ = self.msgs.send(JobMsg::Progress {
            id: self.job.id,
            downloaded,
            total,
            speed_bps,
        });
    }

    /// Stores the job's unfinished files as the tracker has them (only
    /// the media options change), so that removing the job deletes exactly
    /// those, even after a restart.
    fn store_partials(&self, tracker: &Tracker) {
        let Some(mut options) = self.job.media.clone() else {
            return;
        };
        options.partial_files.clone_from(&tracker.partials);
        if let Err(e) = self.store.set_media(self.job.id, &options) {
            tracing::warn!(
                job = self.job.id.0,
                "could not store the partial files: {e}"
            );
        }
    }

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

pub(super) fn emitter(
    events: &broadcast::Sender<Event>,
) -> impl Fn(ToolStatus) + Send + Sync + use<> {
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
