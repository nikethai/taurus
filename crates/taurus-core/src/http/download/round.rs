//! One round of a job's workers: starting them up to the host's
//! connection cap, work stealing, dropping failed URLs, and lowering the
//! cap when a host refuses connections.

use std::path::Path;
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use tokio::task::{JoinError, JoinSet};
use tokio_util::sync::CancellationToken;

use crate::errors::{CoreError, ErrorKind};
use crate::http::sources::Sources;
use crate::plan::{self, Segment};
use crate::progress::ProgressMeter;
use crate::store::JobRecord;

use super::worker::{Worker, WorkerEnd, WorkerExit};
use super::{JobContext, error};

const PROGRESS_TICK: Duration = Duration::from_millis(250);

/// Saves the cap the primary's host allowed during the run, if it refused a
/// connection; the store keeps the lowest one known. Remembering only saves
/// the next job some refusals, so a failure is just logged.
pub(super) fn remember_cap(ctx: &JobContext, job: &JobRecord, limit: &HostLimit) {
    let (Some(host), Some(cap)) = (&limit.host, limit.lowered) else {
        return;
    };
    if let Err(e) = ctx.store.set_host_cap(host, cap) {
        tracing::warn!(
            job = job.id.0,
            "could not remember the limit of {host}: {e}"
        );
    }
}

/// How many workers a job may run against its host: the primary's. Mirrors
/// share the job's cap; they are not capped per host.
pub(super) struct HostLimit {
    /// Where a lowered cap is remembered; `None` if the URL has no host.
    pub(super) host: Option<String>,
    pub(super) cap: usize,
    /// The cap after the primary's last refusal in this run, if any.
    pub(super) lowered: Option<u32>,
}

pub(super) enum RoundEnd {
    /// Every segment is complete.
    Done,
    Cancelled,
    Fallback,
    Failed(CoreError),
}

/// Runs workers for the given segments until all finish or the round stops;
/// `segments` comes back with the durable progress of every worker, `limit`
/// with the cap the host allowed, `sources` with the URLs that failed.
pub(super) async fn run_round(
    ctx: &JobContext,
    job: &JobRecord,
    tapa: &Path,
    segments: &mut Vec<Segment>,
    limit: &mut HostLimit,
    sources: &mut Sources,
) -> RoundEnd {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let open = (0..sources.len()).map(|_| Arc::default()).collect();
    let mut round = Round {
        ctx,
        job,
        tapa,
        slots: segments.iter().copied().map(Slot::new).collect(),
        workers: JoinSet::new(),
        tx,
        stop: ctx.cancel.child_token(),
        end: None,
        limit,
        sources,
        open,
        ramping: true,
    };
    let mut meter = ProgressMeter::new();
    let mut tick = tokio::time::interval(PROGRESS_TICK);
    round.fill_idle_slots();
    while !round.workers.is_empty() {
        tokio::select! {
            // Durable reports first: a worker's last reports precede its exit.
            biased;
            Some(report) = rx.recv() => round.on_report(report),
            Some(joined) = round.workers.join_next() => round.on_exit(joined),
            _ = tick.tick() => round.report_progress(&mut meter),
        }
    }
    while let Ok(report) = rx.try_recv() {
        round.on_report(report);
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
pub(super) struct Window {
    pub(super) pos: u64,
    pub(super) end: u64,
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

pub(super) enum Report {
    /// Segment `idx` is durable up to `done`.
    Durable { idx: usize, done: u64 },
    /// A worker started reading a 206 response.
    Opened,
}

struct Round<'a> {
    ctx: &'a JobContext,
    job: &'a JobRecord,
    tapa: &'a Path,
    slots: Vec<Slot>,
    workers: JoinSet<(usize, WorkerExit)>,
    tx: mpsc::UnboundedSender<Report>,
    /// Stops this round's workers; a child of the job's cancel token.
    stop: CancellationToken,
    /// Why the round stops early; the first reason wins.
    end: Option<RoundEnd>,
    limit: &'a mut HostLimit,
    sources: &'a mut Sources,
    /// The workers' 206 responses currently being read, per source.
    open: Vec<Arc<AtomicUsize>>,
    /// Only one worker runs until a response opens or a worker exits.
    ramping: bool,
}

impl Round<'_> {
    fn segments(&self) -> Vec<Segment> {
        self.slots.iter().map(|s| s.seg).collect()
    }

    fn running(&self) -> usize {
        self.slots.iter().filter(|s| s.running).count()
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

    fn on_report(&mut self, report: Report) {
        match report {
            Report::Durable { idx, done } => {
                self.slots[idx].seg.done = done;
                self.commit();
            }
            Report::Opened if self.ramping => {
                self.ramping = false;
                self.fill_idle_slots();
            }
            Report::Opened => {}
        }
    }

    fn on_exit(&mut self, joined: Result<(usize, WorkerExit), JoinError>) {
        let (idx, exit) = match joined {
            Ok(exit) => exit,
            Err(e) => {
                let detail = format!("download worker crashed: {e}");
                return self.stop_with(RoundEnd::Failed(error(ErrorKind::Unknown, detail)));
            }
        };
        self.ramping = false;
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
            WorkerEnd::ConnectionLimit => {
                // The segment waits, idle, for a worker under the new cap.
                self.lower_cap(self.running() + 1, exit.source);
                self.fill_idle_slots();
            }
            // The primary now serves another version: never stitch it to the
            // bytes already written, whatever the mirrors serve.
            WorkerEnd::SourceFailed(e)
                if exit.source == 0 && e.kind == ErrorKind::FileChangedOnServer =>
            {
                self.stop_with(RoundEnd::Failed(e));
            }
            WorkerEnd::SourceFailed(e) => self.drop_source(exit.source, e),
            // Only the primary's answers decide that the job cannot use ranges.
            WorkerEnd::Fallback if exit.source != 0 => {
                let e = error(ErrorKind::Unknown, "the mirror ignored the Range header");
                self.drop_source(exit.source, e);
            }
            WorkerEnd::Fallback => self.stop_with(RoundEnd::Fallback),
            WorkerEnd::Failed(e) => self.stop_with(RoundEnd::Failed(e)),
        }
    }

    /// Stops using a URL that failed a worker: its segment goes on from the
    /// next good URL, and the round fails once none is left.
    fn drop_source(&mut self, source: usize, e: CoreError) {
        if self.sources.mark_bad(source, e) {
            self.fill_idle_slots();
        } else {
            let e = self.sources.error();
            self.stop_with(RoundEnd::Failed(e));
        }
    }

    /// A host refused a connection while `active` workers (the refused one
    /// included) were running: allow one fewer from now on and, if the
    /// primary refused, note the cap for its host (saved if the job
    /// completes).
    fn lower_cap(&mut self, active: usize, source: usize) {
        let cap = self.limit.cap.min(active.saturating_sub(1).max(1));
        self.limit.cap = cap;
        tracing::info!(
            job = self.job.id.0,
            cap,
            source,
            "host refused a connection"
        );
        // A mirror's refusal says nothing about the primary's host.
        if source != 0 {
            return;
        }
        self.limit.lowered = Some(u32::try_from(cap).unwrap_or(u32::MAX));
    }

    /// Starts workers up to the connection cap (one while ramping up):
    /// unfinished segments first, then (resumable jobs only) new segments
    /// stolen from the largest one.
    fn fill_idle_slots(&mut self) {
        let cap = if self.ramping { 1 } else { self.limit.cap };
        while !self.stop.is_cancelled() && self.running() < cap {
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
        // Lowered before the write so the worker cannot pass the split meanwhile.
        let old_end = {
            let mut w = lock(&self.slots[idx].window);
            // The worker moved past the split since the snapshot: skip this round.
            if split <= w.pos || split >= w.end {
                return Ok(None);
            }
            std::mem::replace(&mut w.end, split)
        };
        let stolen = Segment {
            start: split,
            end: old_end,
            done: 0,
        };
        let mut segments = self.segments();
        segments[idx].end = split;
        segments.push(stolen);
        if let Err(e) = self.ctx.store.replace_segments(self.job.id, &segments) {
            // Nothing was written past the split: hand the bytes back, so
            // memory matches the store again.
            lock(&self.slots[idx].window).end = old_end;
            return Err(e);
        }
        self.slots[idx].seg.end = split;
        self.slots.push(Slot::new(stolen));
        Ok(Some(self.slots.len() - 1))
    }

    fn spawn(&mut self, idx: usize) {
        let Some(source) = self.sources.pick(idx) else {
            let e = self.sources.error();
            return self.stop_with(RoundEnd::Failed(e));
        };
        let slot = &mut self.slots[idx];
        slot.running = true;
        let written = slot.seg.cursor();
        lock(&slot.window).pos = written;
        let worker = Worker {
            idx,
            source,
            client: self.ctx.client.clone(),
            // The link itself, so every request re-follows its redirects.
            url: self.sources.url(source).to_owned(),
            referer: self.job.referer.clone(),
            etag: self.job.etag.clone(),
            last_modified: self.job.last_modified.clone(),
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
            limiter: self.ctx.limiter.clone(),
            timing: self.ctx.timing.clone(),
            open: self.open[source].clone(),
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

pub(super) fn lock(window: &Mutex<Window>) -> MutexGuard<'_, Window> {
    // A worker panic cannot leave the two integers inconsistent.
    window.lock().unwrap_or_else(|p| p.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::limit::Limiter;
    use crate::store::Store;
    use crate::timing::Timing;

    #[tokio::test]
    async fn failed_steal_leaves_memory_as_stored() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("taurus.db")).unwrap();
        let id = store
            .insert_job(&crate::store::NewJob {
                url: "http://127.0.0.1:9/x.bin".into(),
                source_url: "http://127.0.0.1:9/x.bin".into(),
                dest_dir: dir.path().to_path_buf(),
                file_name: "x.bin".into(),
                connections: 2,
                mirrors: Vec::new(),
                checksum: None,
                referer: None,
                queue_pos: 0,
            })
            .unwrap();
        let mut job = store.get_job(id).unwrap().unwrap();
        job.total = Some(4 * plan::MIN_SEGMENT);
        job.resumable = true;
        let whole = Segment {
            start: 0,
            end: 4 * plan::MIN_SEGMENT,
            done: 0,
        };
        store.replace_segments(id, &[whole]).unwrap();
        // Writing segments of a deleted job violates the foreign key.
        store.delete_job(id).unwrap();

        let (progress, _progress_rx) = mpsc::unbounded_channel();
        let ctx = JobContext {
            id,
            client: crate::http::build_client(),
            store,
            cancel: CancellationToken::new(),
            progress,
            limiter: Limiter::new(None),
            timing: Timing::default(),
            auto_checksum: false,
        };
        let (tx, _rx) = mpsc::unbounded_channel();
        let tapa = dir.path().join("x.bin.tapa");
        let mut limit = HostLimit {
            host: None,
            cap: 2,
            lowered: None,
        };
        let mut round = Round {
            ctx: &ctx,
            job: &job,
            tapa: &tapa,
            slots: vec![Slot::new(whole)],
            workers: JoinSet::new(),
            tx,
            stop: CancellationToken::new(),
            end: None,
            limit: &mut limit,
            sources: &mut Sources::new(&job.url, &job.mirrors),
            open: vec![Arc::default()],
            ramping: false,
        };

        assert!(round.steal().is_err());
        assert_eq!(round.segments(), [whole]);
        assert_eq!(round.slots[0].live(), whole);
    }
}
