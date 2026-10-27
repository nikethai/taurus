//! Runs one job: a segmented, resumable download into `<name>.tapa`.
//!
//! One worker per segment writes at its own offset through its own file
//! handle. A worker reports a segment's `done` only after `sync_data`, and the
//! job loop commits it to the store, so recorded progress never exceeds the
//! bytes on disk.

use std::io::SeekFrom;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use reqwest::StatusCode;
use reqwest::header::{CONTENT_RANGE, RANGE};
use tokio::fs::{File, OpenOptions};
use tokio::io::{AsyncSeekExt, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio::task::{JoinError, JoinSet};
use tokio_util::sync::CancellationToken;

use crate::errors::{CoreError, ErrorKind, MAX_SEGMENT_ATTEMPTS, backoff_delay, classify_io};
use crate::filename::{PARTIAL_EXT, unique_path};
use crate::http::client::{request_error, status_error};
use crate::http::probe::parse_content_range;
use crate::plan::{self, Segment};
use crate::progress::ProgressMeter;
use crate::store::{JobId, JobRecord, JobState, Store};

/// A worker syncs and reports progress after this long or this many bytes.
const SYNC_INTERVAL: Duration = Duration::from_secs(1);
const SYNC_BYTES: u64 = 4 * 1024 * 1024;
const PROGRESS_TICK: Duration = Duration::from_millis(250);
const RENAME_ATTEMPTS: u32 = 5;
const RENAME_RETRY_DELAY: Duration = Duration::from_millis(200);

pub struct JobContext {
    pub id: JobId,
    pub client: reqwest::Client,
    pub store: Store,
    pub cancel: CancellationToken,
    /// `(job, downloaded, total, speed in bytes/s)`, at most 4 per second.
    pub progress: mpsc::UnboundedSender<(JobId, u64, Option<u64>, u64)>,
}

#[derive(Debug)]
pub enum JobOutcome {
    Completed(PathBuf),
    Paused,
    Failed(CoreError),
}

/// Downloads the job's remaining bytes and records the resulting state
/// (Completed, Paused or Failed with its error) in the store.
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
    job.error = None;
    // Ranges are only usable against a known size.
    job.resumable &= job.total.is_some();
    ctx.store.update_job(job)?;

    let tapa = job
        .dest_dir
        .join(format!("{}.{PARTIAL_EXT}", job.file_name));
    let mut segments = prepare(ctx, job, &tapa).await?;
    loop {
        let end = run_round(ctx, job, &tapa, &mut segments).await;
        let committed = ctx.store.commit_progress(job.id, &segments);
        if let RoundEnd::Failed(e) = end {
            return Err(e);
        }
        committed?;
        match end {
            RoundEnd::Done => return finish(ctx, job, &tapa).await,
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
        // Keep an earlier run's segments only if they fit the current size.
        Some(total)
            if job.resumable && !stored.is_empty() && stored.iter().all(|s| s.end <= total) =>
        {
            stored
        }
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

/// Verifies the length, then moves the partial to a free final name.
async fn finish(
    ctx: &JobContext,
    job: &mut JobRecord,
    tapa: &Path,
) -> Result<JobOutcome, CoreError> {
    let file = OpenOptions::new()
        .write(true)
        .open(tapa)
        .await
        .map_err(io_error)?;
    let len = file.metadata().await.map_err(io_error)?.len();
    if let Some(total) = job.total.filter(|&total| total != len) {
        return Err(error(
            ErrorKind::Unknown,
            format!("downloaded file is {len} bytes, expected {total}"),
        ));
    }
    file.sync_all().await.map_err(io_error)?;
    drop(file);

    let mut path = job.dest_dir.join(&job.file_name);
    if path.symlink_metadata().is_ok() {
        // The job's own name is in `reserved`; it is taken on disk anyway.
        let mut reserved = ctx.store.reserved_names(&job.dest_dir)?;
        reserved.remove(&job.file_name);
        path = unique_path(&job.dest_dir, &job.file_name, &reserved);
        if let Some(name) = path.file_name() {
            job.file_name = name.to_string_lossy().into_owned();
        }
        ctx.store.update_job(job)?;
    }
    rename_with_retry(tapa, &path).await?;
    job.state = JobState::Completed;
    ctx.store.update_job(job)?;
    Ok(JobOutcome::Completed(path))
}

/// Windows can briefly lock a just-closed file (antivirus, indexer).
async fn rename_with_retry(from: &Path, to: &Path) -> Result<(), CoreError> {
    let mut attempt = 1;
    loop {
        match tokio::fs::rename(from, to).await {
            Ok(()) => return Ok(()),
            Err(e)
                if e.kind() == std::io::ErrorKind::PermissionDenied
                    && attempt < RENAME_ATTEMPTS =>
            {
                attempt += 1;
                tokio::time::sleep(RENAME_RETRY_DELAY).await;
            }
            Err(e) => return Err(io_error(e)),
        }
    }
}

enum RoundEnd {
    /// Every segment is complete.
    Done,
    Cancelled,
    Fallback,
    Failed(CoreError),
}

/// Runs workers for the given segments until all finish or the round stops;
/// `segments` comes back with the durable progress of every worker.
async fn run_round(
    ctx: &JobContext,
    job: &JobRecord,
    tapa: &Path,
    segments: &mut Vec<Segment>,
) -> RoundEnd {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut round = Round {
        ctx,
        job,
        tapa,
        slots: segments.iter().copied().map(Slot::new).collect(),
        workers: JoinSet::new(),
        tx,
        stop: ctx.cancel.child_token(),
        end: None,
    };
    let mut meter = ProgressMeter::new();
    let mut tick = tokio::time::interval(PROGRESS_TICK);
    round.fill_idle_slots();
    while !round.workers.is_empty() {
        tokio::select! {
            // Durable reports first: a worker's last reports precede its exit.
            biased;
            Some(report) = rx.recv() => round.on_durable(report),
            Some(joined) = round.workers.join_next() => round.on_exit(joined),
            _ = tick.tick() => round.report_progress(&mut meter),
        }
    }
    while let Ok(report) = rx.try_recv() {
        round.on_durable(report);
    }
    *segments = round.segments();
    match round.end {
        Some(end) => end,
        None if round.slots.iter().all(|s| s.complete) => RoundEnd::Done,
        None => RoundEnd::Cancelled,
    }
}

/// The byte window a worker may still write: `[pos, end)`. The worker
/// reserves bytes (advances `pos`) under the lock before writing them; work
/// stealing lowers `end` under the same lock and only to a split above `pos`.
/// So no byte is written by two workers and a worker never writes past `end`.
struct Window {
    pos: u64,
    end: u64,
}

struct Slot {
    /// Durable progress and the current end.
    seg: Segment,
    window: Arc<Mutex<Window>>,
    running: bool,
    /// An open-ended segment has bytes "remaining" even when its stream is done.
    complete: bool,
}

impl Slot {
    fn new(seg: Segment) -> Slot {
        Slot {
            window: Arc::new(Mutex::new(Window {
                pos: seg.cursor(),
                end: seg.end,
            })),
            seg,
            running: false,
            complete: seg.remaining() == 0,
        }
    }

    /// The segment with its live (written, not yet durable) progress.
    fn live(&self) -> Segment {
        let w = lock(&self.window);
        Segment {
            start: self.seg.start,
            end: w.end,
            done: w.pos - self.seg.start,
        }
    }
}

struct Durable {
    idx: usize,
    done: u64,
}

struct Round<'a> {
    ctx: &'a JobContext,
    job: &'a JobRecord,
    tapa: &'a Path,
    slots: Vec<Slot>,
    workers: JoinSet<(usize, WorkerExit)>,
    tx: mpsc::UnboundedSender<Durable>,
    /// Stops this round's workers; a child of the job's cancel token.
    stop: CancellationToken,
    /// Why the round stops early; the first reason wins.
    end: Option<RoundEnd>,
}

impl Round<'_> {
    fn segments(&self) -> Vec<Segment> {
        self.slots.iter().map(|s| s.seg).collect()
    }

    fn stop_with(&mut self, end: RoundEnd) {
        self.end.get_or_insert(end);
        self.stop.cancel();
    }

    fn commit(&mut self) {
        if let Err(e) = self
            .ctx
            .store
            .commit_progress(self.job.id, &self.segments())
        {
            self.stop_with(RoundEnd::Failed(e));
        }
    }

    fn on_durable(&mut self, report: Durable) {
        self.slots[report.idx].seg.done = report.done;
        self.commit();
    }

    fn on_exit(&mut self, joined: Result<(usize, WorkerExit), JoinError>) {
        let (idx, exit) = match joined {
            Ok(exit) => exit,
            Err(e) => {
                let detail = format!("download worker crashed: {e}");
                return self.stop_with(RoundEnd::Failed(error(ErrorKind::Unknown, detail)));
            }
        };
        let slot = &mut self.slots[idx];
        slot.running = false;
        slot.seg.done = exit.done;
        self.commit();
        match exit.end {
            WorkerEnd::Complete => {
                self.slots[idx].complete = true;
                self.fill_idle_slots();
            }
            WorkerEnd::Cancelled => {}
            WorkerEnd::Fallback => self.stop_with(RoundEnd::Fallback),
            WorkerEnd::Failed(e) => self.stop_with(RoundEnd::Failed(e)),
        }
    }

    /// Starts workers up to the connection limit: unfinished segments first,
    /// then (resumable jobs only) new segments stolen from the largest one.
    fn fill_idle_slots(&mut self) {
        let max_workers = self.job.connections.max(1) as usize;
        while !self.stop.is_cancelled()
            && self.slots.iter().filter(|s| s.running).count() < max_workers
        {
            let idle = self.slots.iter().position(|s| !s.running && !s.complete);
            let next = match idle {
                Some(idx) => idx,
                None if self.job.resumable => match self.steal() {
                    Ok(Some(idx)) => idx,
                    Ok(None) => break,
                    Err(e) => return self.stop_with(RoundEnd::Failed(e)),
                },
                None => break,
            };
            self.spawn(next);
        }
    }

    /// Splits the segment with the most bytes left (by live position) and
    /// persists the new segment list. `None` if nothing is worth splitting.
    fn steal(&mut self) -> Result<Option<usize>, CoreError> {
        let live: Vec<Segment> = self.slots.iter().map(Slot::live).collect();
        let Some((idx, split)) = plan::steal(&live) else {
            return Ok(None);
        };
        let old_end = {
            let mut w = lock(&self.slots[idx].window);
            // The worker moved past the split since the snapshot: skip this round.
            if split <= w.pos || split >= w.end {
                return Ok(None);
            }
            std::mem::replace(&mut w.end, split)
        };
        self.slots[idx].seg.end = split;
        self.slots.push(Slot::new(Segment {
            start: split,
            end: old_end,
            done: 0,
        }));
        self.ctx
            .store
            .replace_segments(self.job.id, &self.segments())?;
        Ok(Some(self.slots.len() - 1))
    }

    fn spawn(&mut self, idx: usize) {
        let slot = &mut self.slots[idx];
        slot.running = true;
        let written = slot.seg.cursor();
        lock(&slot.window).pos = written;
        let worker = Worker {
            idx,
            client: self.ctx.client.clone(),
            url: self.job.url.clone(),
            tapa: self.tapa.to_path_buf(),
            start: slot.seg.start,
            written,
            synced: written,
            last_sync: Instant::now(),
            window: slot.window.clone(),
            resumable: self.job.resumable,
            total: self.job.total,
            stop: self.stop.clone(),
            tx: self.tx.clone(),
        };
        self.workers.spawn(async move { (idx, worker.run().await) });
    }

    fn report_progress(&self, meter: &mut ProgressMeter) {
        let downloaded = self.slots.iter().map(|s| s.live().done).sum();
        if let Some(speed) = meter.record(Instant::now(), downloaded) {
            let _ = self
                .ctx
                .progress
                .send((self.job.id, downloaded, self.job.total, speed));
        }
    }
}

struct WorkerExit {
    /// Durable progress of the segment when the worker stopped.
    done: u64,
    end: WorkerEnd,
}

enum WorkerEnd {
    Complete,
    Cancelled,
    /// A ranged request got the whole file; the job must restart unranged.
    Fallback,
    Failed(CoreError),
}

struct Worker {
    idx: usize,
    client: reqwest::Client,
    url: String,
    tapa: PathBuf,
    start: u64,
    /// Absolute offsets: bytes written so far, and how many of them are synced.
    written: u64,
    synced: u64,
    last_sync: Instant,
    window: Arc<Mutex<Window>>,
    resumable: bool,
    total: Option<u64>,
    stop: CancellationToken,
    tx: mpsc::UnboundedSender<Durable>,
}

impl Worker {
    async fn run(mut self) -> WorkerExit {
        let end = match OpenOptions::new().write(true).open(&self.tapa).await {
            Ok(file) => self.download(file).await,
            Err(e) => WorkerEnd::Failed(io_error(e)),
        };
        WorkerExit {
            done: self.synced - self.start,
            end,
        }
    }

    /// Fetches until the window is complete, retrying transient errors from
    /// the durable position with backoff.
    async fn download(&mut self, mut file: File) -> WorkerEnd {
        let mut attempt = 0;
        loop {
            let begin = self.written;
            let fetched = self.fetch(&mut file).await;
            // Keep whatever arrived, even on error: it is the resume point.
            let synced = self.sync(&mut file).await;
            let e = match (fetched, synced) {
                (Ok(end), Ok(())) => return end,
                (Err(e), Ok(())) => e,
                // A non-transient fetch error (e.g. a failed write) explains a failed sync.
                (Err(e), Err(_)) if !e.kind.is_transient() => e,
                (_, Err(e)) => e,
            };
            if self.written > begin {
                attempt = 0;
            }
            attempt += 1;
            if !e.kind.is_transient() || attempt >= MAX_SEGMENT_ATTEMPTS {
                return WorkerEnd::Failed(e);
            }
            tracing::debug!(segment = self.idx, attempt, "retrying: {e}");
            tokio::select! {
                _ = self.stop.cancelled() => return WorkerEnd::Cancelled,
                _ = tokio::time::sleep(backoff_delay(attempt - 1)) => {}
            }
            if !self.resumable {
                if let Err(e) = self.restart(&file).await {
                    return WorkerEnd::Failed(e);
                }
            }
        }
    }

    /// One request, streamed into the file from `written` up to the window end.
    async fn fetch(&mut self, file: &mut File) -> Result<WorkerEnd, CoreError> {
        let end = lock(&self.window).end;
        if self.written >= end {
            return Ok(WorkerEnd::Complete);
        }
        let mut request = self.client.get(&self.url);
        if self.resumable {
            request = request.header(RANGE, format!("bytes={}-{}", self.written, end - 1));
        }
        let mut response = tokio::select! {
            _ = self.stop.cancelled() => return Ok(WorkerEnd::Cancelled),
            sent = request.send() => sent.map_err(request_error)?,
        };
        if let Some(end) = self.check_response(&response, end)? {
            return Ok(end);
        }
        file.seek(SeekFrom::Start(self.written))
            .await
            .map_err(io_error)?;
        loop {
            let chunk = tokio::select! {
                _ = self.stop.cancelled() => return Ok(WorkerEnd::Cancelled),
                chunk = response.chunk() => chunk.map_err(body_error)?,
            };
            let Some(chunk) = chunk else { break };
            let take = self.reserve(chunk.len() as u64);
            file.write_all(&chunk[..take as usize])
                .await
                .map_err(io_error)?;
            self.written += take;
            if self.written >= lock(&self.window).end {
                return Ok(WorkerEnd::Complete);
            }
            if self.written - self.synced >= SYNC_BYTES || self.last_sync.elapsed() >= SYNC_INTERVAL
            {
                self.sync(file).await?;
            }
        }
        // The body ended before the window did. Only an unranged stream of
        // unknown (or reached) size is complete; anything else was cut short.
        let complete = !self.resumable && self.total.is_none_or(|t| self.written >= t);
        if complete {
            Ok(WorkerEnd::Complete)
        } else {
            Err(error(
                ErrorKind::ConnectionLost,
                format!("server closed the response early at byte {}", self.written),
            ))
        }
    }

    /// Accepts the response, asks for the fallback, or returns the error.
    fn check_response(
        &self,
        response: &reqwest::Response,
        end: u64,
    ) -> Result<Option<WorkerEnd>, CoreError> {
        let status = response.status();
        if !status.is_success() {
            return Err(status_error(status));
        }
        match status {
            StatusCode::OK if !self.resumable => Ok(None),
            StatusCode::PARTIAL_CONTENT if self.resumable => {
                let range = response
                    .headers()
                    .get(CONTENT_RANGE)
                    .and_then(|v| v.to_str().ok())
                    .and_then(parse_content_range);
                let (first, total) = range.map_or((None, None), |r| (r.first, r.total));
                if total.is_some() && total != self.total {
                    // Another size means another file: never stitch two versions.
                    return Err(error(
                        ErrorKind::FileChangedOnServer,
                        format!(
                            "server now reports {total:?} bytes, expected {:?}",
                            self.total
                        ),
                    ));
                }
                if first == Some(self.written) {
                    Ok(None)
                } else {
                    Err(error(
                        ErrorKind::ConnectionLost,
                        format!("asked for byte {}, server sent {first:?}", self.written),
                    ))
                }
            }
            // A full body is usable only if this segment is the whole file.
            StatusCode::OK if self.written == 0 && self.total.is_some_and(|t| end >= t) => Ok(None),
            StatusCode::OK => Ok(Some(WorkerEnd::Fallback)),
            _ => Err(status_error(status)),
        }
    }

    /// Claims up to `len` bytes of the window; fewer at its end.
    fn reserve(&self, len: u64) -> u64 {
        let mut w = lock(&self.window);
        let take = len.min(w.end.saturating_sub(w.pos));
        w.pos += take;
        take
    }

    /// Makes the written bytes durable, then reports them.
    async fn sync(&mut self, file: &mut File) -> Result<(), CoreError> {
        if self.written == self.synced {
            return Ok(());
        }
        file.flush().await.map_err(io_error)?;
        file.sync_data().await.map_err(io_error)?;
        self.synced = self.written;
        self.last_sync = Instant::now();
        self.report();
        Ok(())
    }

    /// An unranged stream cannot resume: drop what it wrote and start at 0.
    /// The zero is reported before truncating; a job that is not resumable
    /// never resumes from recorded progress, so the order is safe.
    async fn restart(&mut self, file: &File) -> Result<(), CoreError> {
        lock(&self.window).pos = 0;
        self.written = 0;
        self.synced = 0;
        self.report();
        file.set_len(0).await.map_err(io_error)
    }

    fn report(&self) {
        let _ = self.tx.send(Durable {
            idx: self.idx,
            done: self.synced - self.start,
        });
    }
}

fn lock(window: &Mutex<Window>) -> MutexGuard<'_, Window> {
    // A worker panic cannot leave the two integers inconsistent.
    window.lock().unwrap_or_else(|p| p.into_inner())
}

fn error(kind: ErrorKind, detail: impl Into<String>) -> CoreError {
    CoreError {
        kind,
        detail: detail.into(),
    }
}

/// reqwest reports a failed body read as a decode error, which
/// `classify_reqwest` cannot place; with identity encoding it is the
/// connection failing (or timing out) mid-body.
fn body_error(e: reqwest::Error) -> CoreError {
    let mut err = request_error(e);
    if err.kind != ErrorKind::Timeout {
        err.kind = ErrorKind::ConnectionLost;
    }
    err
}

fn io_error(e: std::io::Error) -> CoreError {
    error(classify_io(&e), e.to_string())
}
