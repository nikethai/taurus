//! Runs one job: a segmented, resumable download into `<name>.tapa`.
//!
//! One worker per segment writes at its own offset through its own file
//! handle. A worker reports a segment's `done` only after `sync_data`, and the
//! job loop commits it to the store, so recorded progress never exceeds the
//! bytes on disk.
//!
//! A host that refuses extra connections (429, 503 or 403 while another of
//! the job's responses from the same URL is being served) lowers the job's
//! worker cap instead of failing it; the primary's cap is remembered per
//! host for the next job once the job completes (a run that fails may have
//! been refused for another reason, e.g. an expiring link). A round
//! starts one worker and the rest once that one is served, so the host's
//! first refusals already come while a response is open.
//!
//! A job may have mirrors. Segment `i` starts on URL `i % len` of
//! `[url] + mirrors`. A URL that fails a worker for good (a non-transient
//! error, or out of attempts) is dropped for the run and the segment goes on
//! from the next good URL; the job fails only when every URL is dropped,
//! with the primary's error. Every 206 must match the job's size and
//! validators before any of its bytes is written, so another version of the
//! file (or a mirror serving another file) is never stitched in.

mod finalize;
mod part_file;
mod round;
mod window;
mod worker;

use std::path::{Path, PathBuf};

use tokio::fs::OpenOptions;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::errors::{CoreError, ErrorKind, classify_io};
use crate::filename::PARTIAL_EXT;
use crate::http::sources::Sources;
use crate::limit::Limiter;
use crate::plan::{self, Segment};
use crate::store::{JobId, JobRecord, JobState, Store};
use crate::timing::Timing;

use finalize::finish;
use round::{HostLimit, RoundEnd, remember_cap, run_round};

pub struct JobContext {
    pub id: JobId,
    pub client: reqwest::Client,
    pub store: Store,
    pub cancel: CancellationToken,
    /// `(job, downloaded, total, speed in bytes/s)`, at most 4 per second.
    pub progress: mpsc::UnboundedSender<(JobId, u64, Option<u64>, u64)>,
    /// Shared with every other job: the user's speed limit.
    pub limiter: Limiter,
    pub timing: Timing,
    /// Look for the file's sum in a `SHA256SUMS` file next to it when the
    /// job names none.
    pub auto_checksum: bool,
}

/// The key a host's learned connection limit is stored under: the host,
/// lower-cased, plus `:port` when the URL names a port. `None` without a host.
pub fn host_key(url: &str) -> Option<String> {
    let url = url::Url::parse(url).ok()?;
    let host = url.host_str()?.to_ascii_lowercase();
    Some(match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host,
    })
}

#[derive(Debug)]
pub enum JobOutcome {
    Completed(PathBuf),
    Paused,
    Failed(CoreError),
}

/// Downloads the job's remaining bytes and records the resulting state
/// (Completed, Paused or Failed with its error) in the store. The job's
/// recorded error is the caller's to set or clear before a run; it is
/// replaced only if the run fails.
pub async fn run_job(ctx: JobContext) -> JobOutcome {
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

async fn drive(ctx: &JobContext, job: &mut JobRecord) -> Result<JobOutcome, CoreError> {
    job.state = JobState::Downloading;
    // Ranges are only usable against a known size.
    job.resumable &= job.total.is_some();
    ctx.store.update_job(job)?;

    let tapa = job
        .dest_dir
        .join(format!("{}.{PARTIAL_EXT}", job.file_name));
    let mut segments = prepare(ctx, job, &tapa).await?;
    let host = host_key(job.final_url.as_deref().unwrap_or(&job.url));
    let learned = match &host {
        Some(host) => ctx.store.host_cap(host)?,
        None => None,
    };
    let mut limit = HostLimit {
        host,
        cap: job.connections.min(learned.unwrap_or(u32::MAX)).max(1) as usize,
        lowered: None,
    };
    let mut sources = Sources::new(&job.url, &job.mirrors);
    loop {
        let end = run_round(ctx, job, &tapa, &mut segments, &mut limit, &mut sources).await;
        let committed = ctx.store.commit_progress(job.id, &segments);
        if let RoundEnd::Failed(e) = end {
            return Err(e);
        }
        committed?;
        match end {
            RoundEnd::Done => {
                let outcome = finish(ctx, job, &tapa).await?;
                if matches!(outcome, JobOutcome::Completed(_)) {
                    remember_cap(ctx, job, &limit);
                }
                return Ok(outcome);
            }
            RoundEnd::Fallback => {
                segments = fall_back(ctx, job, &tapa).await?;
                if !ctx.cancel.is_cancelled() {
                    continue;
                }
            }
            RoundEnd::Cancelled | RoundEnd::Failed(_) => {}
        }
        job.state = JobState::Paused;
        ctx.store.update_job(job)?;
        return Ok(JobOutcome::Paused);
    }
}

/// Creates the partial file and decides the segments to download.
async fn prepare(
    ctx: &JobContext,
    job: &JobRecord,
    tapa: &Path,
) -> Result<Vec<Segment>, CoreError> {
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(tapa)
        .await
        .map_err(io_error)?;
    let on_disk = file.metadata().await.map_err(io_error)?.len();
    let stored = ctx.store.load_segments(job.id)?;
    let mut segments = match job.total {
        Some(0) => plan::plan_segments(0, 1),
        // Keep an earlier run's segments only if they tile the current size.
        Some(total) if job.resumable && tiles(&stored, total) => stored,
        Some(total) if job.resumable => plan::plan_segments(total, job.connections),
        // Without ranges a stream can only start over from byte 0.
        _ => vec![open_ended()],
    };
    // Progress past the end of the file means the partial was lost or replaced.
    if segments.iter().any(|s| s.done > 0 && s.cursor() > on_disk) {
        segments.iter_mut().for_each(|s| s.done = 0);
    }
    if segments.iter().all(|s| s.done == 0) {
        file.set_len(0).await.map_err(io_error)?;
    }
    ctx.store.replace_segments(job.id, &segments)?;
    Ok(segments)
}

/// Whether the segments, ordered by start, cover `[0, total)` exactly once.
fn tiles(segments: &[Segment], total: u64) -> bool {
    let mut sorted = segments.to_vec();
    sorted.sort_by_key(|s| s.start);
    let mut next = 0;
    for s in &sorted {
        if s.start != next || s.end < s.start {
            return false;
        }
        next = s.end;
    }
    !sorted.is_empty() && next == total
}

fn open_ended() -> Segment {
    Segment {
        start: 0,
        end: u64::MAX,
        done: 0,
    }
}

/// The server ignored a Range header: restart as one unranged stream from 0.
/// The store is updated before the file is truncated.
async fn fall_back(
    ctx: &JobContext,
    job: &mut JobRecord,
    tapa: &Path,
) -> Result<Vec<Segment>, CoreError> {
    tracing::info!(
        job = job.id.0,
        "server ignored Range; restarting as one stream"
    );
    job.resumable = false;
    ctx.store.update_job(job)?;
    let segments = vec![open_ended()];
    ctx.store.replace_segments(job.id, &segments)?;
    let file = OpenOptions::new()
        .write(true)
        .open(tapa)
        .await
        .map_err(io_error)?;
    file.set_len(0).await.map_err(io_error)?;
    file.sync_all().await.map_err(io_error)?;
    Ok(segments)
}

fn error(kind: ErrorKind, detail: impl Into<String>) -> CoreError {
    CoreError {
        kind,
        detail: detail.into(),
    }
}

fn io_error(e: std::io::Error) -> CoreError {
    error(classify_io(&e), e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_key_is_lowercase_host_and_explicit_port() {
        assert_eq!(
            host_key("https://CDN.Example.com/a.bin").as_deref(),
            Some("cdn.example.com")
        );
        assert_eq!(
            host_key("http://127.0.0.1:8080/a.bin").as_deref(),
            Some("127.0.0.1:8080")
        );
        // The scheme's default port is not explicit.
        assert_eq!(
            host_key("https://example.com:443/a").as_deref(),
            Some("example.com")
        );
        assert_eq!(host_key("not a url"), None);
    }
}
