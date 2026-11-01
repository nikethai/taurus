//! Runs one torrent job: gets a magnet link's metadata (and places the job's
//! files), refuses unsafe torrents and duplicates, checks the free space,
//! adds the torrent to the session and reports its progress until every
//! selected file is downloaded, then shares it until the seeding policy says
//! stop.
//!
//! Like the HTTP runner, it records the job's state as it goes: Downloading,
//! Seeding, then Completed, Paused or Failed with its error. It never keeps a
//! torrent handle across a wait, so neither a finished run nor the hub's
//! shutdown is left holding the torrent's files open.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use librqbit::{TorrentStats, TorrentStatsState};
use tokio::sync::mpsc;
use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;

use super::files::{check_paths, output_dir, selected_total, stored_name, stored_output_dir};
use super::hub::{TorrentHub, info_from_bytes};
use super::policy::{SeedPolicy, classify_torrent_error};
use super::source::classify;
use super::{TorrentFile, TorrentInfo};
use crate::engine::{JobMsg, TorrentView};
use crate::errors::{CoreError, ErrorKind};
use crate::http::JobOutcome;
use crate::settings::TorrentSettings;
use crate::store::{JobId, JobRecord, JobState, Store};
use crate::system::{SpaceSource, check_fits, mark_downloaded};
use crate::timing::Timing;

/// How often the uploaded bytes are saved while the torrent runs; they are
/// also saved whenever the run ends.
const COMMIT_EVERY: Duration = Duration::from_secs(5);

pub struct TorrentJobContext {
    pub id: JobId,
    pub hub: Arc<TorrentHub>,
    pub store: Store,
    pub timing: Timing,
    /// The seeding policy is read from here.
    pub settings: TorrentSettings,
    /// Where free space is read from.
    pub space: SpaceSource,
    /// Pause, removal or shutdown: the torrent is paused and the run ends.
    pub cancel: CancellationToken,
    /// `Progress` and `TorrentStats` every `timing.torrent_poll` (at most 4
    /// per second), and `Seeding` once the download is done.
    pub msgs: mpsc::UnboundedSender<JobMsg>,
}

/// How a run that added the torrent ended, short of an error.
enum End {
    Completed,
    Cancelled,
}

/// Runs the job until the seeding policy stops it (Completed), it is
/// cancelled (Paused) or it fails, and records that state in the store. The
/// job's recorded error is the caller's to set or clear before a run; it is
/// replaced only if the run fails.
pub async fn run_torrent_job(ctx: TorrentJobContext) -> JobOutcome {
    let mut job = match ctx.store.get_job(ctx.id) {
        Ok(Some(job)) => job,
        Ok(None) => return JobOutcome::Failed(error(ErrorKind::Unknown, "job not found")),
        Err(e) => return JobOutcome::Failed(e),
    };
    match drive(&ctx, &mut job).await {
        Ok(outcome) => outcome,
        Err(e) => {
            job.state = JobState::Failed;
            job.error = Some((e.kind, e.detail.clone()));
            if let Err(store_err) = ctx.store.update_job(&job) {
                tracing::warn!(job = job.id.0, "could not record failure: {store_err}");
            }
            JobOutcome::Failed(e)
        }
    }
}

async fn drive(ctx: &TorrentJobContext, job: &mut JobRecord) -> Result<JobOutcome, CoreError> {
    let Some(info) = metadata(ctx, job).await? else {
        return paused(ctx, job);
    };
    let total = selected_total(&info, job.only_files.as_deref());
    job.state = JobState::Downloading;
    job.total = Some(total);
    ctx.store.update_job(job)?;
    preflight(ctx, job, &info, total)?;
    if ctx.cancel.is_cancelled() {
        return paused(ctx, job);
    }

    let output = stored_output_dir(&job.dest_dir, &job.file_name, &info);
    let meta = job.torrent_meta.as_deref().unwrap_or_default();
    let handle = ctx
        .hub
        .add_bytes(meta, &output, job.only_files.clone(), false)
        .await?;
    // Already in the session (restored by the hub, or left by an earlier
    // run), it may be paused.
    let was_paused = handle.is_paused();
    drop(handle);
    if was_paused {
        tokio::select! {
            unpaused = ctx.hub.unpause(&info.info_hash) => unpaused?,
            () = ctx.cancel.cancelled() => {}
        }
    }

    let mut uploads = Uploads::new(job.uploaded);
    let end = watch(ctx, job, &info, &output, total, &mut uploads).await;
    // Whatever ended the run, sharing stops with it.
    if let Err(e) = ctx.hub.pause(&info.info_hash).await {
        tracing::warn!(job = job.id.0, "could not pause the torrent: {e}");
    }
    let committed = ctx.store.commit_torrent_progress(job.id, uploads.total());
    let end = end?;
    committed?;
    match end {
        End::Completed => {
            job.state = JobState::Completed;
            ctx.store.update_job(job)?;
            Ok(JobOutcome::Completed(output))
        }
        End::Cancelled => paused(ctx, job),
    }
}

fn paused(ctx: &TorrentJobContext, job: &mut JobRecord) -> Result<JobOutcome, CoreError> {
    job.state = JobState::Paused;
    ctx.store.update_job(job)?;
    Ok(JobOutcome::Paused)
}

/// The torrent's metadata, checked to be safe. A job without any (a magnet
/// link, or a `.torrent` file not read yet) gets it from the session, and is
/// placed in the same store update that saves it: its output folder chosen
/// and its `file_name` set to the entry that folder (or single file) takes
/// in `dest_dir`. `None` if cancelled meanwhile.
async fn metadata(
    ctx: &TorrentJobContext,
    job: &mut JobRecord,
) -> Result<Option<TorrentInfo>, CoreError> {
    if let Some(meta) = &job.torrent_meta {
        let info = info_from_bytes(meta)?;
        check_paths(&info.files)?;
        return Ok(Some(info));
    }
    let source = classify(&job.source_url)?;
    let (info, bytes) = tokio::select! {
        listed = ctx.hub.list_files(&source, ctx.timing.torrent_metadata_timeout) => listed?,
        () = ctx.cancel.cancelled() => return Ok(None),
    };
    check_paths(&info.files)?;
    if let Some(other) = ctx.store.find_by_info_hash(&info.info_hash)?
        && other != job.id
    {
        return Err(error(
            ErrorKind::AlreadyAdded,
            format!("this torrent is already added as job {}", other.0),
        ));
    }
    // The job's own name is reserved too, unless another job shares it.
    let mut reserved = ctx.store.reserved_names(&job.dest_dir)?;
    if !ctx
        .store
        .name_used_by_other(job.id, &job.dest_dir, &job.file_name)?
    {
        reserved.remove(&job.file_name);
    }
    let output = output_dir(&job.dest_dir, &info, &reserved);
    let mut placed = job.clone();
    placed.file_name = stored_name(&job.dest_dir, &output, &info);
    placed.info_hash = Some(info.info_hash.clone());
    placed.torrent_meta = Some(bytes);
    placed.total = Some(selected_total(&info, job.only_files.as_deref()));
    // Another job saving the same info hash first is `AlreadyAdded` here.
    ctx.store.update_job(&placed)?;
    *job = placed;
    Ok(Some(info))
}

/// The selected bytes still to download must fit in `dest_dir`, and the
/// largest selected file on its file system. A torrent that already
/// finished once (it is only shared again) needs nothing.
fn preflight(
    ctx: &TorrentJobContext,
    job: &JobRecord,
    info: &TorrentInfo,
    total: u64,
) -> Result<(), CoreError> {
    let have = if job.seed_started_at.is_some() {
        total
    } else {
        // Only a torrent the session has checked knows what is on disk.
        ctx.hub.handle(&info.info_hash).map_or(0, |handle| {
            let stats = handle.stats();
            match stats.state {
                TorrentStatsState::Live | TorrentStatsState::Paused => stats.progress_bytes,
                _ => 0,
            }
        })
    };
    let remaining = total.saturating_sub(have);
    if remaining == 0 {
        return Ok(());
    }
    let largest = selected(info, job).map(|f| f.size).max();
    check_fits(ctx.space, &job.dest_dir, 0, remaining, largest)
}

/// Polls the torrent every `torrent_poll`, reporting its progress and saving
/// its uploads, until the seeding policy stops it or the run is cancelled.
/// Once every selected file is downloaded, they are marked as downloaded,
/// the seeding start is recorded and, unless the policy already says stop,
/// the job turns Seeding.
async fn watch(
    ctx: &TorrentJobContext,
    job: &mut JobRecord,
    info: &TorrentInfo,
    output: &Path,
    total: u64,
    uploads: &mut Uploads,
) -> Result<End, CoreError> {
    let policy = SeedPolicy {
        stop_ratio: ctx.settings.stop_ratio,
        stop_after: ctx.settings.stop_after_secs.map(Duration::from_secs),
    };
    let mut tick = tokio::time::interval(ctx.timing.torrent_poll);
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut committed_at = Instant::now();
    let mut seeding = false;
    loop {
        tokio::select! {
            biased;
            () = ctx.cancel.cancelled() => return Ok(End::Cancelled),
            _ = tick.tick() => {}
        }
        // Not kept past this poll: see the module doc.
        let (torrent, stats) = match ctx.hub.handle(&info.info_hash) {
            Some(handle) => (handle.id(), handle.stats()),
            None => {
                return Err(error(
                    ErrorKind::Unknown,
                    format!("torrent {} left the session", info.info_hash),
                ));
            }
        };
        if let Some(e) = stats.error {
            return Err(error(classify_torrent_error(&e), e));
        }
        let uploaded = uploads.observe(torrent, stats.uploaded_bytes);
        report(ctx, job, info, &stats, uploaded, total);
        if committed_at.elapsed() >= COMMIT_EVERY {
            ctx.store.commit_torrent_progress(job.id, uploaded)?;
            committed_at = Instant::now();
        }
        if !stats.finished {
            continue;
        }
        if !seeding {
            for file in selected(info, job) {
                mark_downloaded(
                    &file_path(output, file),
                    &job.source_url,
                    job.referer.as_deref(),
                );
            }
            job.seed_started_at.get_or_insert_with(unix_now);
        }
        let started = job.seed_started_at.unwrap_or_else(unix_now);
        let seeding_for = Duration::from_secs(u64::try_from(unix_now() - started).unwrap_or(0));
        if policy.should_stop(uploaded, total, seeding_for) {
            return Ok(End::Completed);
        }
        if !seeding {
            seeding = true;
            job.state = JobState::Seeding;
            ctx.store.update_job(job)?;
            let _ = ctx.msgs.send(JobMsg::Seeding { id: job.id });
        }
    }
}

/// Sends the torrent's progress (once librqbit has checked it, so that the
/// numbers cover the selected files) and its stats.
fn report(
    ctx: &TorrentJobContext,
    job: &JobRecord,
    info: &TorrentInfo,
    stats: &TorrentStats,
    uploaded: u64,
    total: u64,
) {
    let live = stats.live.as_ref();
    if matches!(
        stats.state,
        TorrentStatsState::Live | TorrentStatsState::Paused
    ) {
        let _ = ctx.msgs.send(JobMsg::Progress {
            id: job.id,
            downloaded: stats.progress_bytes,
            total: Some(stats.total_bytes),
            speed_bps: live.map_or(0, |l| l.download_speed.as_bytes()),
        });
    }
    let view = TorrentView {
        peers: live.map_or(0, |l| l.snapshot.peer_stats.live),
        uploaded,
        upload_bps: live.map_or(0, |l| l.upload_speed.as_bytes()),
        ratio: if total > 0 {
            uploaded as f64 / total as f64
        } else {
            0.0
        },
        files_selected: count(selected(info, job).count()),
        files_total: count(info.files.len()),
    };
    let _ = ctx.msgs.send(JobMsg::TorrentStats { id: job.id, view });
}

/// The files the job downloads.
fn selected<'a>(
    info: &'a TorrentInfo,
    job: &'a JobRecord,
) -> impl Iterator<Item = &'a TorrentFile> {
    info.files.iter().filter(|f| {
        job.only_files
            .as_ref()
            .is_none_or(|only| only.contains(&f.index))
    })
}

fn file_path(output: &Path, file: &TorrentFile) -> PathBuf {
    file.path
        .split('/')
        .fold(output.to_path_buf(), |p, c| p.join(c))
}

fn count(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// Bytes uploaded in all: what earlier runs saved plus what librqbit counted
/// in this one. librqbit's counter restarts from 0 each time the torrent
/// goes live again (or is added again as a new torrent), so a drop, or
/// another torrent id, starts a new stretch counted from 0. The first value
/// seen is the baseline: anything counted before this run was saved by the
/// run that counted it.
struct Uploads {
    /// Saved by earlier runs, plus this run's finished stretches.
    before: u64,
    torrent: Option<usize>,
    baseline: u64,
    last: u64,
}

impl Uploads {
    fn new(saved: u64) -> Uploads {
        Uploads {
            before: saved,
            torrent: None,
            baseline: 0,
            last: 0,
        }
    }

    /// Takes librqbit's counter of the torrent with id `torrent`; returns
    /// the new total.
    fn observe(&mut self, torrent: usize, counter: u64) -> u64 {
        match self.torrent {
            None => self.baseline = counter,
            Some(seen) if seen != torrent || counter < self.last => {
                self.before = self.total();
                self.baseline = 0;
            }
            Some(_) => {}
        }
        self.torrent = Some(torrent);
        self.last = counter;
        self.total()
    }

    fn total(&self) -> u64 {
        self.before
            .saturating_add(self.last.saturating_sub(self.baseline))
    }
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

fn error(kind: ErrorKind, detail: impl Into<String>) -> CoreError {
    CoreError {
        kind,
        detail: detail.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uploads_add_up_across_resets() {
        let mut up = Uploads::new(1_000);
        // Counted before this run (and saved by it): the baseline.
        assert_eq!(up.observe(1, 300), 1_000);
        assert_eq!(up.observe(1, 500), 1_200);
        // A pause and unpause restarts librqbit's counter.
        assert_eq!(up.observe(1, 0), 1_200);
        assert_eq!(up.observe(1, 50), 1_250);
        // A new torrent object counts from 0 too, even with a higher value.
        assert_eq!(up.observe(2, 70), 1_320);
        assert_eq!(up.observe(2, 70), 1_320);
        assert_eq!(up.total(), 1_320);
    }

    #[test]
    fn uploads_start_at_the_saved_count() {
        let mut up = Uploads::new(42);
        assert_eq!(up.total(), 42);
        assert_eq!(up.observe(7, 0), 42);
        assert_eq!(up.observe(7, 8), 50);
    }
}
