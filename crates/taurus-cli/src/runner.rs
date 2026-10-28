//! The queue runner behind `add`, `resume`, `run` and `start-now`: enqueues,
//! then follows the engine's events until the queue is idle and the
//! when-done action has fired or been cancelled. Ctrl-C pauses.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::future::Future;
use std::io::{IsTerminal, Write};
use std::process::ExitCode;
use std::time::Duration;

use taurus_core::{Engine, Event, JobId, JobState, JobView, Timing, WhenDone};
use tokio::sync::broadcast::{self, error::RecvError, error::TryRecvError};
use tokio::time::Instant;

use crate::Result;
use crate::render;

/// How the runner prints.
pub struct Options {
    /// Every event as one JSON line, nothing else.
    pub json: bool,
    /// When-done commands only would have run.
    pub dry_run: bool,
    /// The engine's timing, to tell when a Waiting job retries.
    pub timing: Timing,
}

/// What the enqueue step did.
#[derive(Default)]
pub struct Enqueued {
    pub ids: Vec<JobId>,
    /// Some job could not be enqueued (already reported on stderr).
    pub failed: bool,
}

pub struct Runner<'a> {
    engine: &'a Engine,
    events: broadcast::Receiver<Event>,
    /// Every job's state, as the events tell it.
    states: HashMap<JobId, JobState>,
    /// The jobs this run is about: busy when it began, or enqueued by it.
    touched: BTreeSet<JobId>,
    /// A job was busy since the last `QueueIdle`, so another one comes.
    idle_due: bool,
    /// The queue went idle and its when-done action has not yet fired or
    /// been cancelled.
    when_done_due: bool,
    /// The last when-done to fire was Notify, and nothing ran since.
    notify: bool,
    out: Output,
}

impl<'a> Runner<'a> {
    /// Subscribes before anything is enqueued, so no event is missed.
    pub fn new(engine: &'a Engine, options: Options) -> Runner<'a> {
        let events = engine.subscribe();
        let views = engine.snapshot();
        let states: HashMap<JobId, JobState> = views.iter().map(|v| (v.id, v.state)).collect();
        let touched: BTreeSet<JobId> = views
            .iter()
            .filter(|v| is_busy(v.state))
            .map(|v| v.id)
            .collect();
        let mut out = Output::new(options);
        for view in &views {
            out.seed(view);
        }
        Runner {
            engine,
            events,
            idle_due: !touched.is_empty(),
            states,
            touched,
            when_done_due: false,
            notify: false,
            out,
        }
    }

    /// Whether some job was queued, waiting or downloading when the runner
    /// began.
    pub fn has_work(&self) -> bool {
        !self.touched.is_empty()
    }

    /// Runs `enqueue`, then follows the queue until it is idle. Exit code:
    /// 2 after Ctrl-C (running jobs are paused, queued ones stay queued);
    /// 1 if something could not be enqueued or a touched job Failed; 0 when
    /// every touched job Completed.
    pub async fn run(mut self, enqueue: impl Future<Output = Enqueued>) -> Result<ExitCode> {
        let ctrl_c = tokio::signal::ctrl_c();
        tokio::pin!(ctrl_c, enqueue);
        let mut listening = true;
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        let mut interrupted = false;
        // Events are printed while enqueueing, but only judged after.
        let enqueued = loop {
            tokio::select! {
                enqueued = &mut enqueue => break Some(enqueued),
                event = self.events.recv() => self.received(event)?,
                signal = &mut ctrl_c, if listening => match signal {
                    Ok(()) => break None,
                    // No Ctrl-C handler on this platform: keep going.
                    Err(_) => listening = false,
                },
            }
        };
        let mut failed = false;
        match enqueued {
            Some(enqueued) => {
                failed = enqueued.failed;
                for id in enqueued.ids {
                    self.touch(id);
                }
            }
            None => interrupted = true,
        }
        while !interrupted {
            self.drain()?;
            if self.done() {
                break;
            }
            tokio::select! {
                event = self.events.recv() => self.received(event)?,
                signal = &mut ctrl_c, if listening => match signal {
                    // Only a when-done countdown left to stop.
                    Ok(()) if !self.busy() && !self.idle_due => {
                        self.engine.cancel_when_done().await?;
                        // A finished future must not be polled again.
                        ctrl_c.set(tokio::signal::ctrl_c());
                    }
                    Ok(()) => interrupted = true,
                    Err(_) => listening = false,
                },
                _ = tick.tick(), if self.out.ticks() => self.out.redraw()?,
            }
        }
        if interrupted {
            self.engine.shutdown().await?;
        }
        // The jobs that the shutdown paused.
        self.drain()?;
        self.finish(interrupted, failed)
    }

    /// Adds an enqueued job to the run.
    fn touch(&mut self, id: JobId) {
        self.touched.insert(id);
        if !self.states.contains_key(&id)
            && let Some(view) = self.engine.snapshot().into_iter().find(|v| v.id == id)
        {
            self.out.seed(&view);
            self.states.insert(id, view.state);
            if is_busy(view.state) {
                self.idle_due = true;
            }
        }
    }

    /// Done once nothing is busy, the `QueueIdle` that says so has come and
    /// any when-done action has run.
    fn done(&self) -> bool {
        !self.busy() && !self.idle_due && !self.when_done_due
    }

    fn busy(&self) -> bool {
        self.states.values().any(|s| is_busy(*s))
    }

    /// Handles the events already queued.
    fn drain(&mut self) -> Result<()> {
        loop {
            match self.events.try_recv() {
                Ok(event) => self.event(event)?,
                Err(TryRecvError::Lagged(_)) => self.resync(),
                Err(TryRecvError::Empty | TryRecvError::Closed) => return Ok(()),
            }
        }
    }

    fn received(&mut self, event: std::result::Result<Event, RecvError>) -> Result<()> {
        match event {
            Ok(event) => self.event(event),
            Err(RecvError::Lagged(_)) => {
                self.resync();
                Ok(())
            }
            // The engine never closes the channel while it lives.
            Err(RecvError::Closed) => Ok(()),
        }
    }

    fn event(&mut self, event: Event) -> Result<()> {
        let before = match &event {
            Event::JobAdded(view) | Event::JobUpdated(view) => self.states.get(&view.id).copied(),
            _ => None,
        };
        self.out.event(&event, before)?;
        match event {
            Event::JobAdded(view) | Event::JobUpdated(view) => {
                self.states.insert(view.id, view.state);
            }
            Event::JobRemoved(id) => {
                self.states.remove(&id);
            }
            Event::QueueIdle => {
                self.idle_due = false;
                // Only jobs finishing idle the runner's queue, so the engine
                // runs the action right after announcing idle, or once the
                // enqueue step's hold is dropped.
                self.when_done_due = self.engine.settings().when_done != WhenDone::Nothing;
            }
            Event::WhenDoneCancelled => self.when_done_due = false,
            Event::WhenDoneFired { action, .. } => {
                self.when_done_due = false;
                self.notify = action == WhenDone::Notify;
            }
            Event::Progress { .. }
            | Event::SettingsChanged(_)
            | Event::WhenDoneCountdown { .. } => {}
        }
        if self.busy() {
            self.idle_due = true;
            self.notify = false;
        }
        Ok(())
    }

    /// Events were missed: starts over from the engine's views.
    fn resync(&mut self) {
        self.states = self
            .engine
            .snapshot()
            .into_iter()
            .map(|v| (v.id, v.state))
            .collect();
        self.idle_due = self.busy();
    }

    fn finish(mut self, interrupted: bool, failed: bool) -> Result<ExitCode> {
        let views: Vec<JobView> = self
            .engine
            .snapshot()
            .into_iter()
            .filter(|v| self.touched.contains(&v.id))
            .collect();
        for view in &views {
            self.out.outcome(view)?;
        }
        if self.notify && !interrupted {
            self.out.line(render::ALL_FINISHED)?;
        }
        self.out.clear()?;
        let states: Vec<JobState> = views.iter().map(|v| v.state).collect();
        Ok(exit_code(interrupted, failed, &states))
    }
}

fn exit_code(interrupted: bool, failed: bool, states: &[JobState]) -> ExitCode {
    if interrupted {
        ExitCode::from(2)
    } else if failed
        || states
            .iter()
            .any(|s| *s != JobState::Completed && *s != JobState::Paused)
    {
        ExitCode::FAILURE
    } else if states.contains(&JobState::Paused) {
        ExitCode::from(2)
    } else {
        ExitCode::SUCCESS
    }
}

fn is_busy(state: JobState) -> bool {
    matches!(
        state,
        JobState::Downloading | JobState::Queued | JobState::Waiting
    )
}

/// Writes the events to stdout: JSON lines with `--json`; otherwise one
/// line per running job, redrawn in place on a terminal and printed per
/// update when piped, with outcomes and when-done lines above them.
struct Output {
    json: bool,
    tty: bool,
    dry_run: bool,
    timing: Timing,
    names: HashMap<JobId, String>,
    /// The running and waiting jobs, in id order.
    jobs: BTreeMap<JobId, Status>,
    /// A when-done countdown: the action and the seconds left.
    countdown: Option<(WhenDone, u64)>,
    /// Lines of the block on screen (terminal only).
    drawn: usize,
    /// Jobs whose outcome is printed.
    reported: HashSet<JobId>,
    waits: HashMap<JobId, Wait>,
}

enum Status {
    Progress {
        downloaded: u64,
        total: Option<u64>,
        speed_bps: u64,
    },
    Waiting {
        retry_at: Instant,
    },
}

/// Mirrors the engine's waiting retries: the first after
/// `waiting_retry_first`, doubling per wait without progress.
struct Wait {
    count: u32,
    downloaded: u64,
}

impl Output {
    fn new(options: Options) -> Output {
        Output {
            json: options.json,
            tty: std::io::stdout().is_terminal(),
            dry_run: options.dry_run,
            timing: options.timing,
            names: HashMap::new(),
            jobs: BTreeMap::new(),
            countdown: None,
            drawn: 0,
            reported: HashSet::new(),
            waits: HashMap::new(),
        }
    }

    /// Learns a job from a view that was not announced.
    fn seed(&mut self, view: &JobView) {
        self.names.insert(view.id, view.file_name.clone());
        if view.state == JobState::Downloading {
            self.jobs.insert(view.id, progress(view));
        }
    }

    /// Whether a Waiting job's countdown needs redrawing every second.
    fn ticks(&self) -> bool {
        self.tty
            && !self.json
            && self
                .jobs
                .values()
                .any(|s| matches!(s, Status::Waiting { .. }))
    }

    /// `before` is the job's state before a JobAdded or JobUpdated.
    fn event(&mut self, event: &Event, before: Option<JobState>) -> Result<()> {
        if self.json {
            let mut out = std::io::stdout().lock();
            writeln!(out, "{}", serde_json::to_string(event)?)?;
            // Flushed per event: readers may be following a pipe live.
            out.flush()?;
            return Ok(());
        }
        match event {
            Event::JobAdded(view) | Event::JobUpdated(view) => {
                self.names.insert(view.id, view.file_name.clone());
                match view.state {
                    JobState::Downloading => self.show(view.id, progress(view))?,
                    JobState::Waiting => {
                        let retry_at = self.retry_at(view);
                        self.show(view.id, Status::Waiting { retry_at })?;
                    }
                    JobState::Queued => self.hide(view.id)?,
                    JobState::Completed | JobState::Paused | JobState::Failed => {
                        self.waits.remove(&view.id);
                        self.hide(view.id)?;
                        if before.is_some_and(is_busy) {
                            self.outcome(view)?;
                        }
                    }
                }
            }
            Event::Progress {
                id,
                downloaded,
                total,
                speed_bps,
            } => self.show(
                *id,
                Status::Progress {
                    downloaded: *downloaded,
                    total: *total,
                    speed_bps: *speed_bps,
                },
            )?,
            Event::JobRemoved(id) => self.hide(*id)?,
            Event::WhenDoneCountdown {
                action,
                seconds_left,
            } => {
                self.countdown = Some((*action, *seconds_left));
                if self.tty {
                    self.redraw()?;
                } else {
                    self.line(&render::countdown_line(*action, *seconds_left))?;
                }
            }
            Event::WhenDoneCancelled => {
                if let Some((action, _)) = self.countdown.take() {
                    self.line(&render::cancelled_line(action))?;
                }
            }
            Event::WhenDoneFired { action, command } => {
                self.countdown = None;
                if let Some(line) = render::fired_line(*action, command.as_deref(), self.dry_run) {
                    self.line(&line)?;
                }
            }
            Event::QueueIdle | Event::SettingsChanged(_) => {}
        }
        Ok(())
    }

    /// When a job that has just started waiting retries.
    fn retry_at(&mut self, view: &JobView) -> Instant {
        let wait = self.waits.entry(view.id).or_insert(Wait {
            count: 0,
            downloaded: view.downloaded,
        });
        if view.downloaded > wait.downloaded {
            wait.count = 0;
        }
        wait.downloaded = view.downloaded;
        let delay = self
            .timing
            .waiting_retry_first
            .saturating_mul(2u32.saturating_pow(wait.count))
            .min(self.timing.waiting_retry_cap);
        wait.count = wait.count.saturating_add(1);
        Instant::now() + delay
    }

    /// Prints the job's outcome once (human mode only).
    fn outcome(&mut self, view: &JobView) -> Result<()> {
        if self.json || !self.reported.insert(view.id) {
            return Ok(());
        }
        self.line(&render::outcome_line(view))
    }

    fn show(&mut self, id: JobId, status: Status) -> Result<()> {
        if self.tty {
            self.jobs.insert(id, status);
            self.redraw()
        } else {
            let line = self.job_line(id, &status);
            let old = self.jobs.insert(id, status);
            if old.is_some_and(|old| self.job_line(id, &old) == line) {
                return Ok(());
            }
            self.line(&line)
        }
    }

    fn hide(&mut self, id: JobId) -> Result<()> {
        if self.jobs.remove(&id).is_some() && self.tty {
            self.redraw()?;
        }
        Ok(())
    }

    fn job_line(&self, id: JobId, status: &Status) -> String {
        let text = match status {
            Status::Progress {
                downloaded,
                total,
                speed_bps,
            } => render::progress_line(*downloaded, *total, *speed_bps),
            Status::Waiting { retry_at } => {
                render::waiting_line(retry_at.saturating_duration_since(Instant::now()))
            }
        };
        let name = self.names.get(&id).map_or("", String::as_str);
        render::job_line(id, name, &text)
    }

    /// Prints a line above the block (human mode only).
    fn line(&mut self, line: &str) -> Result<()> {
        if self.json {
            return Ok(());
        }
        self.clear()?;
        let mut out = std::io::stdout().lock();
        writeln!(out, "{line}")?;
        out.flush()?;
        drop(out);
        if self.tty {
            self.redraw()?;
        }
        Ok(())
    }

    /// Draws the block again in place (terminal only).
    fn redraw(&mut self) -> Result<()> {
        if !self.tty || self.json {
            return Ok(());
        }
        let mut lines: Vec<String> = self
            .jobs
            .iter()
            .map(|(id, status)| self.job_line(*id, status))
            .collect();
        if let Some((action, seconds_left)) = self.countdown {
            lines.push(render::countdown_line(action, seconds_left));
        }
        let mut out = std::io::stdout().lock();
        if self.drawn > 0 {
            write!(out, "\x1b[{}A", self.drawn)?;
        }
        write!(out, "\r\x1b[J")?;
        for line in &lines {
            writeln!(out, "{line}")?;
        }
        out.flush()?;
        self.drawn = lines.len();
        Ok(())
    }

    /// Erases the block (terminal only).
    fn clear(&mut self) -> Result<()> {
        if self.drawn > 0 {
            let mut out = std::io::stdout().lock();
            write!(out, "\x1b[{}A\r\x1b[J", self.drawn)?;
            out.flush()?;
            self.drawn = 0;
        }
        Ok(())
    }
}

fn progress(view: &JobView) -> Status {
    Status::Progress {
        downloaded: view.downloaded,
        total: view.total,
        speed_bps: view.speed_bps,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn code(interrupted: bool, failed: bool, states: &[JobState]) -> ExitCode {
        exit_code(interrupted, failed, states)
    }

    #[test]
    fn exit_codes() {
        use JobState::*;
        assert_eq!(code(false, false, &[]), ExitCode::SUCCESS);
        assert_eq!(
            code(false, false, &[Completed, Completed]),
            ExitCode::SUCCESS
        );
        assert_eq!(code(false, false, &[Completed, Failed]), ExitCode::FAILURE);
        assert_eq!(code(false, true, &[Completed]), ExitCode::FAILURE);
        assert_eq!(code(false, false, &[Completed, Paused]), ExitCode::from(2));
        assert_eq!(code(false, false, &[Paused, Failed]), ExitCode::FAILURE);
        assert_eq!(code(true, false, &[Completed]), ExitCode::from(2));
        assert_eq!(code(true, true, &[Failed]), ExitCode::from(2));
    }
}
