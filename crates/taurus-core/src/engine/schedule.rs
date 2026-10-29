//! The queue: starting Queued jobs while fewer than `max_active` run,
//! retrying Waiting jobs, and what follows every change (`QueueIdle`, the
//! when-done action, keep-awake, answering waiters).

use std::sync::atomic::Ordering;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::errors::{CoreError, ErrorKind};
use crate::http::{JobContext, JobOutcome, run_job};
use crate::store::{JobId, JobState};
use crate::timing::Timing;

use super::actor::{Actor, JobMsg, Retry, Run, is_final};
use super::jobs::{check_space, downloaded};
use super::{Event, no_such_job, read, write};

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
        self.start(id);
        self.after_change();
        Ok(())
    }

    /// Runs after every change of a job's state, of the settings or of the
    /// holds: starts Queued jobs in queue order while fewer than
    /// `max_active` download (failing, instead, one whose rest no longer
    /// fits on its disk beside the running jobs' rest), announces
    /// `QueueIdle` when the last busy job leaves and, if a job ending made
    /// it leave, runs the when-done action once nothing holds it (only with
    /// `autostart`), keeps the computer awake while a job downloads, then
    /// answers the waiters of jobs that are done (after their events).
    pub(super) fn after_change(&mut self) {
        if !self.stopping {
            self.schedule();
            let busy = read(&self.views).values().any(|v| {
                matches!(
                    v.state,
                    JobState::Downloading | JobState::Queued | JobState::Waiting
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
            .any(|v| v.state == JobState::Downloading);
        self.awake.set(self.settings.keep_awake && downloading);
        self.keeping_awake
            .store(self.awake.is_requested(), Ordering::Relaxed);
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
        let max_active = usize::try_from(self.settings.max_active).unwrap_or(usize::MAX);
        if self.running.len() >= max_active {
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
            if self.running.len() >= max_active {
                break;
            }
            if !self.running.contains_key(&id) && self.state_of(id) == Some(JobState::Queued) {
                match self.fits_to_start(id) {
                    Ok(()) => self.start(id),
                    // Not transient: it waits for the user to free space and
                    // resume it. A smaller job further on may still fit.
                    Err(e) => self.record_failure(id, e),
                }
            }
        }
    }

    /// The pre-flight check again, as the job is about to start: the rest of
    /// it must fit beside what the running jobs on its disk still download.
    /// A job whose size is unknown, or that cannot be read, is let through.
    fn fits_to_start(&self, id: JobId) -> Result<(), CoreError> {
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
    pub(super) fn retry_later(&mut self, id: JobId, progressed: bool) {
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
    fn requeue(&mut self, id: JobId) -> Result<(), CoreError> {
        self.store.requeue(id)?;
        if let Some(view) = write(&self.views).get_mut(&id) {
            view.error = None;
        }
        self.set_view_state(id, JobState::Queued);
        Ok(())
    }

    /// Spawns the job's download task; the job is Downloading from here on.
    fn start(&mut self, id: JobId) {
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
        };
        let tx = self.job_tx.clone();
        tokio::spawn(async move {
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
