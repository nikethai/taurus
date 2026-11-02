//! The queue: starting Queued jobs while fewer than `max_active` HTTP jobs
//! (and `torrent.max_active` downloading torrents) run, retrying Waiting
//! jobs, the free-space check as a job starts, and what follows every change
//! (`QueueIdle`, the when-done action, keep-awake, rate limits, answering
//! waiters).

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::errors::{CoreError, ErrorKind};
use crate::http::{JobContext, JobOutcome, run_job};
use crate::store::{JobId, JobKind, JobState};
use crate::system::{SpaceSource, check_fits, same_volume};
use crate::timing::Timing;

use super::actor::{Actor, JobMsg, Retry, Run, is_final};
use super::jobs::downloaded;
use super::settings::unix_now;
use super::{Event, Views, no_such_job, read, write};

impl Actor {
    pub(super) fn start_now(&mut self, id: JobId) -> Result<(), CoreError> {
        let state = self.state_of(id).ok_or_else(no_such_job)?;
        if self.running.contains_key(&id) {
            return Ok(());
        }
        if !matches!(state, JobState::Queued | JobState::Waiting) {
            return Ok(());
        }
        self.cancel_retry(id);
        if let Err(e) = self.fits_to_start(id) {
            self.fail(id, e.clone());
            return Err(e);
        }
        if state == JobState::Waiting {
            self.requeue(id)?;
        }
        self.launch(id);
        self.after_change();
        Ok(())
    }

    /// Runs after every change of a job's state, of the settings or of the
    /// holds: starts Queued jobs in queue order while fewer than
    /// `max_active` HTTP jobs (`torrent.max_active` torrents) download
    /// (failing, instead, an HTTP job whose rest no longer fits on its disk
    /// beside the running jobs' rest), announces
    /// `QueueIdle` when the last busy job leaves and, if a job ending made
    /// it leave, runs the when-done action once nothing holds it (only with
    /// `autostart`), keeps the computer awake while a job downloads, splits
    /// the speed limit between the kinds downloading, then answers the
    /// waiters of jobs that are done (after their events).
    pub(super) fn after_change(&mut self) {
        if !self.stopping {
            self.schedule();
            // Seeding is not busy: sharing must not hold up the idle queue.
            let busy = read(&self.views).values().any(|v| {
                matches!(
                    v.state,
                    JobState::Downloading
                        | JobState::Recording
                        | JobState::Queued
                        | JobState::Waiting
                )
            });
            if self.busy && !busy {
                self.emit(Event::QueueIdle);
                self.idle_pending |= self.autostart && self.job_ended;
            }
            self.busy = busy;
            // Busy again: the next idle queue decides.
            self.idle_pending &= !busy;
            if self.idle_pending && self.holds.load(Ordering::SeqCst) == 0 {
                self.idle_pending = false;
                self.when_done.queue_idle(self.settings.when_done);
            }
        }
        self.job_ended = false;
        let downloading = read(&self.views)
            .values()
            .any(|v| matches!(v.state, JobState::Downloading | JobState::Recording));
        self.awake.set(self.settings.keep_awake && downloading);
        self.keeping_awake
            .store(self.awake.is_requested(), Ordering::Relaxed);
        self.apply_limits();
        let views = read(&self.views);
        self.waiters.retain(|id, waiters| {
            let state = match views.get(id) {
                Some(view) if !is_final(view.state) => return true,
                Some(view) => view.state,
                // Removed.
                None => JobState::Failed,
            };
            for waiter in waiters.drain(..) {
                let _ = waiter.send(state);
            }
            false
        });
    }

    fn schedule(&mut self) {
        if !self.autostart {
            return;
        }
        let max_http = usize::try_from(self.settings.max_active).unwrap_or(usize::MAX);
        let max_torrents = usize::try_from(self.settings.torrent.max_active).unwrap_or(usize::MAX);
        let (mut http, mut torrents) = self.active();
        let full = |http: usize, torrents: usize| http >= max_http && torrents >= max_torrents;
        if full(http, torrents) {
            return;
        }
        let queued = match self.store.queued_jobs() {
            Ok(queued) => queued,
            Err(e) => {
                tracing::warn!("could not read the queue: {e}");
                return;
            }
        };
        for id in queued {
            if full(http, torrents) {
                break;
            }
            if self.running.contains_key(&id) || self.state_of(id) != Some(JobState::Queued) {
                continue;
            }
            if self.is_torrent(id) {
                if torrents < max_torrents {
                    torrents += 1;
                    self.start_torrent(id);
                }
            } else if http < max_http {
                match self.fits_to_start(id) {
                    Ok(()) => {
                        http += 1;
                        self.launch(id);
                    }
                    // Not transient: it waits for the user to free space and
                    // resume it. A smaller job further on may still fit.
                    Err(e) => self.record_failure(id, e),
                }
            }
        }
    }

    /// Running HTTP, media and convert jobs, and running torrents that are not
    /// Seeding: what `max_active` and `torrent.max_active` limit.
    pub(super) fn active(&self) -> (usize, usize) {
        let views = read(&self.views);
        let running = self.running.keys().filter_map(|id| views.get(id));
        running.fold((0, 0), |(http, torrents), view| match view.kind {
            JobKind::Http | JobKind::Media | JobKind::Convert => (http + 1, torrents),
            JobKind::Torrent if view.state == JobState::Seeding => (http, torrents),
            JobKind::Torrent => (http, torrents + 1),
        })
    }

    /// The pre-flight check again, as the job is about to start: the rest of
    /// it must fit beside what the running jobs on its disk still download.
    /// A job whose size is unknown, that has nothing left to download (only
    /// the check and the rename; see `check_space`), or that cannot be read,
    /// is let through; so is a torrent, whose runner checks for itself.
    fn fits_to_start(&self, id: JobId) -> Result<(), CoreError> {
        if self.is_torrent(id) {
            return Ok(());
        }
        let read_job = self
            .store
            .get_job(id)
            .and_then(|job| Ok((job, downloaded(&self.store, id)?)));
        let (job, done) = match read_job {
            Ok((Some(job), done)) => (job, done),
            Ok((None, _)) => return Ok(()),
            Err(e) => {
                tracing::warn!(job = id.0, "could not read the job to check its space: {e}");
                return Ok(());
            }
        };
        match job.total {
            Some(total) => check_space(
                &self.views,
                self.space,
                &job.dest_dir,
                total.saturating_sub(done),
                Some(total),
                Some(id),
            ),
            None => Ok(()),
        }
    }

    /// Retries the Waiting job after `waiting_retry_first`, doubling per wait
    /// in a row in which it made no progress, up to `waiting_retry_cap`.
    /// Returns when the retry is due, in unix seconds (rounded up).
    pub(super) fn retry_later(&mut self, id: JobId, progressed: bool) -> i64 {
        let count = self.retry_counts.entry(id).or_insert(0);
        if progressed {
            *count = 0;
        }
        let delay = retry_delay(&self.timing, *count);
        *count = count.saturating_add(1);
        self.next_retry_seq += 1;
        let seq = self.next_retry_seq;
        let tx = self.job_tx.clone();
        let timer = tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            let _ = tx.send(JobMsg::RetryDue { id, seq });
        })
        .abort_handle();
        if let Some(old) = self.retries.insert(id, Retry { seq, timer }) {
            old.timer.abort();
        }
        let secs = i64::try_from(delay.as_secs_f64().ceil() as u64).unwrap_or(i64::MAX);
        unix_now().saturating_add(secs)
    }

    pub(super) fn on_retry_due(&mut self, id: JobId, seq: u64) {
        // A cancelled timer may have fired before it was aborted.
        if self.retries.get(&id).is_none_or(|r| r.seq != seq) {
            return;
        }
        self.retries.remove(&id);
        if self.state_of(id) == Some(JobState::Waiting) {
            if let Err(e) = self.requeue(id) {
                tracing::warn!(job = id.0, "could not queue the waiting job: {e}");
            }
            self.after_change();
        }
    }

    pub(super) fn cancel_retry(&mut self, id: JobId) {
        if let Some(retry) = self.retries.remove(&id) {
            retry.timer.abort();
        }
    }

    /// Queues the job again, clearing its error.
    pub(super) fn requeue(&mut self, id: JobId) -> Result<(), CoreError> {
        self.store.requeue(id)?;
        if let Some(view) = write(&self.views).get_mut(&id) {
            view.error = None;
        }
        self.set_view_state(id, JobState::Queued);
        Ok(())
    }

    /// Starts the job; one that was interrupted (`Store::requeue_interrupted`)
    /// first has its link as added resolved again and checked like `resume`
    /// does, and starts once that is done (`on_resolved`). The job is
    /// Downloading, and holds its slot, from here on. A torrent has no link
    /// to check: its runner starts at once.
    fn launch(&mut self, id: JobId) {
        if self.is_torrent(id) {
            return self.start_torrent(id);
        }
        let interrupted = self.store.needs_revalidation(id).and_then(|marked| {
            if marked {
                self.store.get_job(id)
            } else {
                Ok(None)
            }
        });
        match interrupted {
            Ok(Some(job)) => {
                if let Err(e) = self.store.set_state(id, JobState::Downloading) {
                    tracing::warn!(job = id.0, "could not record the start: {e}");
                }
                let downloaded = read(&self.views).get(&id).map_or(0, |v| v.downloaded);
                tracing::info!(job = id.0, "checking the interrupted job's link again");
                self.spawn_resolve(job, downloaded);
                self.set_view_state(id, JobState::Downloading);
            }
            Ok(None) => self.start(id),
            Err(e) => {
                tracing::warn!(job = id.0, "could not read whether the job is checked: {e}");
                self.start(id);
            }
        }
    }

    /// Spawns the job's download task; the job is Downloading from here on.
    /// A media job has a runner of its own (`start_media`), and so has a
    /// convert job (`start_convert`).
    pub(super) fn start(&mut self, id: JobId) {
        if self.is_media(id) {
            return self.start_media(id);
        }
        if self.is_convert(id) {
            return self.start_convert(id);
        }
        if let Err(e) = self.store.set_state(id, JobState::Downloading) {
            // The run records its own state as it starts.
            tracing::warn!(job = id.0, "could not record the start: {e}");
        }
        let downloaded_at_start = read(&self.views).get(&id).map_or(0, |v| v.downloaded);
        let cancel = CancellationToken::new();
        let (progress, mut progress_rx) = mpsc::unbounded_channel();
        let ctx = JobContext {
            id,
            client: self.client.clone(),
            store: self.store.clone(),
            cancel: cancel.clone(),
            progress,
            limiter: self.limiter.clone(),
            timing: self.timing.clone(),
            auto_checksum: self.settings.auto_checksum,
            auth: None,
        };
        let tx = self.job_tx.clone();
        let secrets = self.secrets.clone();
        tokio::spawn(async move {
            let mut ctx = ctx;
            ctx.auth = secrets.auth_for_stored(&ctx.store, id).await;
            // A task of its own, so a panic still ends in `Finished`.
            let job = tokio::spawn(run_job(ctx));
            // Ends when the job drops its progress sender, i.e. when it ends.
            let forward = async {
                while let Some((id, downloaded, total, speed_bps)) = progress_rx.recv().await {
                    let _ = tx.send(JobMsg::Progress {
                        id,
                        downloaded,
                        total,
                        speed_bps,
                    });
                }
            };
            let (joined, ()) = tokio::join!(job, forward);
            let outcome = joined.unwrap_or_else(|e| {
                JobOutcome::Failed(CoreError {
                    kind: ErrorKind::Unknown,
                    detail: format!("download task crashed: {e}"),
                })
            });
            let _ = tx.send(JobMsg::Finished { id, outcome });
        });
        self.running.insert(
            id,
            Run {
                cancel,
                downloaded_at_start,
            },
        );
        self.set_view_state(id, JobState::Downloading);
    }
}

/// The pre-flight check: `remaining` more bytes of a `total`-byte file
/// must fit in `dir` beside what every Downloading job on the same disk
/// (other than `except`) still has to download. Nothing left to download
/// (a job stopped while its file was checked) needs no space.
pub(super) fn check_space(
    views: &Views,
    space: SpaceSource,
    dir: &Path,
    remaining: u64,
    total: Option<u64>,
    except: Option<JobId>,
) -> Result<(), CoreError> {
    if remaining == 0 {
        return Ok(());
    }
    check_fits(
        space,
        dir,
        reserved_bytes(views, dir, except),
        remaining,
        total,
    )
}

/// What every Downloading job (other than `except`) on the disk holding
/// `dir` still has to download.
pub(super) fn reserved_bytes(views: &Views, dir: &Path, except: Option<JobId>) -> u64 {
    let running: Vec<(PathBuf, u64)> = read(views)
        .values()
        .filter(|v| v.state == JobState::Downloading && Some(v.id) != except)
        .filter_map(|v| Some((v.dest_dir.clone(), v.total?.saturating_sub(v.downloaded))))
        .collect();
    running
        .iter()
        .filter(|(other, _)| same_volume(other, dir))
        .fold(0u64, |sum, (_, left)| sum.saturating_add(*left))
}

/// Delay before a Waiting job's retry after `count` earlier waits in a row:
/// `waiting_retry_first` doubled `count` times, at most `waiting_retry_cap`.
fn retry_delay(timing: &Timing, count: u32) -> Duration {
    timing
        .waiting_retry_first
        .saturating_mul(2u32.saturating_pow(count))
        .min(timing.waiting_retry_cap)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_delay_doubles_up_to_the_cap() {
        let timing = Timing::default();
        let delays: Vec<u64> = [0, 1, 2, 3, 4, 5, 40]
            .into_iter()
            .map(|count| retry_delay(&timing, count).as_secs())
            .collect();
        assert_eq!(delays, [60, 120, 240, 480, 600, 600, 600]);
    }
}
