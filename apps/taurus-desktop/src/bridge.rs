//! Engine events to UI updates: the `Coalescer` keeps the latest state and
//! hands it out at most once per interval, so a burst of progress events
//! costs the UI one refresh.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};
use taurus_core::{Engine, Event, JobId, JobView, Settings, WhenDone};
use tokio::sync::broadcast::error::RecvError;

/// Feeds the engine's events into `coalescer` on the runtime until the
/// engine's event channel closes. Subscribes, then seeds the coalescer with
/// the engine's snapshot, so no job is missed; after falling behind the
/// channel (events were dropped) it seeds again from a fresh snapshot.
pub fn spawn(
    engine: &Engine,
    rt: &tokio::runtime::Handle,
    coalescer: Arc<Mutex<Coalescer>>,
) -> tokio::task::JoinHandle<()> {
    let mut events = engine.subscribe();
    lock(&coalescer).seed(engine.snapshot());
    let engine = engine.clone();
    rt.spawn(async move {
        loop {
            match events.recv().await {
                Ok(event) => lock(&coalescer).push(event),
                Err(RecvError::Lagged(_)) => lock(&coalescer).seed(engine.snapshot()),
                Err(RecvError::Closed) => break,
            }
        }
    })
}

/// The coalescer, also after a panic elsewhere while it was locked (its
/// state stays consistent: every method leaves it whole).
pub fn lock(c: &Mutex<Coalescer>) -> std::sync::MutexGuard<'_, Coalescer> {
    c.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What the when-done action is doing, from the newest such event.
#[derive(Clone, Debug, PartialEq)]
pub enum WhenDoneUi {
    Countdown { action: WhenDone, seconds: u64 },
    Cancelled,
    Fired { action: WhenDone },
}

/// One refresh for the UI.
#[derive(Clone, Debug)]
pub struct Flush {
    /// Every known job, in id order (the engine's snapshot order).
    pub views: Vec<JobView>,
    /// Jobs were added or removed, or one changed state, since the last
    /// flush: the tabs need rebuilding. Otherwise only numbers moved.
    /// (Addition to the brief's `Flush`; lets the UI skip the tab split.)
    pub structural: bool,
    /// The newest settings, when they changed since the last flush.
    pub settings: Option<Settings>,
    /// The newest when-done event; the newest one wins within a flush.
    pub when_done: Option<WhenDoneUi>,
}

pub struct Coalescer {
    min_interval: Duration,
    views: BTreeMap<JobId, JobView>,
    dirty: bool,
    structural: bool,
    settings: Option<Settings>,
    when_done: Option<WhenDoneUi>,
    last_flush: Option<Instant>,
}

impl Coalescer {
    pub fn new(min_interval: Duration) -> Self {
        Self {
            min_interval,
            views: BTreeMap::new(),
            dirty: false,
            structural: false,
            settings: None,
            when_done: None,
            last_flush: None,
        }
    }

    /// Replaces the known jobs with `views`, from `engine.snapshot()`. Call
    /// it after subscribing, so no event is missed (an event older than the
    /// snapshot just replaces a view with an equal or older one, and the
    /// next event for that job corrects it), and again after missing
    /// events. The next flush carries them all and is structural.
    pub fn seed(&mut self, views: Vec<JobView>) {
        self.views.clear();
        for v in views {
            self.views.insert(v.id, v);
        }
        self.dirty = true;
        self.structural = true;
    }

    pub fn push(&mut self, event: Event) {
        match event {
            Event::JobAdded(v) | Event::JobUpdated(v) => {
                let changed_state = self.views.get(&v.id).is_none_or(|old| old.state != v.state);
                self.structural |= changed_state;
                self.views.insert(v.id, v);
            }
            // A Progress for an unknown job is dropped, not kept: the
            // JobAdded / JobUpdated view that follows (or the seed) already
            // carries the numbers.
            Event::Progress {
                id,
                downloaded,
                total,
                speed_bps,
            } => {
                let Some(v) = self.views.get_mut(&id) else {
                    return;
                };
                v.downloaded = downloaded;
                v.total = total;
                v.speed_bps = speed_bps;
            }
            Event::JobRemoved(id) => {
                if self.views.remove(&id).is_none() {
                    return;
                }
                self.structural = true;
            }
            Event::SettingsChanged(s) => self.settings = Some(s),
            Event::QueueIdle => return,
            Event::WhenDoneCountdown {
                action,
                seconds_left,
            } => {
                self.when_done = Some(WhenDoneUi::Countdown {
                    action,
                    seconds: seconds_left,
                })
            }
            Event::WhenDoneCancelled => self.when_done = Some(WhenDoneUi::Cancelled),
            Event::WhenDoneFired { action, .. } => {
                self.when_done = Some(WhenDoneUi::Fired { action })
            }
        }
        self.dirty = true;
    }

    /// The pending refresh, when there is one and `min_interval` has passed
    /// since the last one (the first goes out at once).
    pub fn take_if_due(&mut self, now: Instant) -> Option<Flush> {
        if !self.dirty {
            return None;
        }
        if let Some(last) = self.last_flush
            && now.saturating_duration_since(last) < self.min_interval
        {
            return None;
        }
        self.last_flush = Some(now);
        self.dirty = false;
        Some(Flush {
            views: self.views.values().cloned().collect(),
            structural: std::mem::take(&mut self.structural),
            settings: self.settings.take(),
            when_done: self.when_done.take(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::view;
    use taurus_core::JobState;

    fn progress(id: i64, downloaded: u64) -> Event {
        Event::Progress {
            id: JobId(id),
            downloaded,
            total: Some(1000),
            speed_bps: downloaded * 2,
        }
    }

    #[test]
    fn coalescer_keeps_latest_and_limits_rate() {
        let t0 = Instant::now();
        let mut c = Coalescer::new(Duration::from_millis(100));
        assert!(c.take_if_due(t0).is_none());
        c.push(Event::JobAdded(view(1, "a", JobState::Downloading)));
        let first = c.take_if_due(t0).unwrap();
        assert!(first.structural);
        for i in 1..=500 {
            c.push(progress(1, i));
        }
        // inside the interval: held back
        assert!(c.take_if_due(t0 + Duration::from_millis(50)).is_none());
        assert!(c.take_if_due(t0 + Duration::from_millis(99)).is_none());
        let f = c.take_if_due(t0 + Duration::from_millis(100)).unwrap();
        assert_eq!(f.views.len(), 1);
        assert_eq!((f.views[0].downloaded, f.views[0].speed_bps), (500, 1000));
        assert!(!f.structural);
        assert!(f.settings.is_none() && f.when_done.is_none());
        // nothing new: nothing due
        assert!(c.take_if_due(t0 + Duration::from_secs(5)).is_none());
        c.push(progress(1, 600));
        assert!(c.take_if_due(t0 + Duration::from_millis(150)).is_none());
        assert!(c.take_if_due(t0 + Duration::from_millis(200)).is_some());
    }

    #[test]
    fn coalescer_events() {
        let t0 = Instant::now();
        let mut c = Coalescer::new(Duration::from_millis(100));
        c.seed(vec![
            view(2, "b", JobState::Queued),
            view(1, "a", JobState::Paused),
        ]);
        let f = c.take_if_due(t0).unwrap();
        assert_eq!(f.views.iter().map(|v| v.id.0).collect::<Vec<_>>(), [1, 2]);
        assert!(f.structural);

        // progress for an unknown job is ignored and flushes nothing
        c.push(progress(99, 5));
        assert!(c.take_if_due(t0 + Duration::from_secs(1)).is_none());
        c.push(Event::QueueIdle);
        assert!(c.take_if_due(t0 + Duration::from_secs(1)).is_none());

        // an update without a state change is not structural; with one it is
        let mut u = view(1, "a", JobState::Paused);
        u.downloaded = 7;
        c.push(Event::JobUpdated(u));
        let f = c.take_if_due(t0 + Duration::from_secs(1)).unwrap();
        assert!(!f.structural);
        assert_eq!(f.views[0].downloaded, 7);
        c.push(Event::JobUpdated(view(1, "a", JobState::Downloading)));
        assert!(
            c.take_if_due(t0 + Duration::from_secs(2))
                .unwrap()
                .structural
        );

        // removal drops the view and is structural
        c.push(Event::JobRemoved(JobId(2)));
        let f = c.take_if_due(t0 + Duration::from_secs(3)).unwrap();
        assert!(f.structural);
        assert_eq!(f.views.len(), 1);
        c.push(Event::JobRemoved(JobId(2)));
        assert!(c.take_if_due(t0 + Duration::from_secs(4)).is_none());

        // seeding again (after missed events) replaces the known jobs
        c.seed(vec![view(3, "c", JobState::Queued)]);
        let f = c.take_if_due(t0 + Duration::from_secs(5)).unwrap();
        assert!(f.structural);
        assert_eq!(f.views.iter().map(|v| v.id.0).collect::<Vec<_>>(), [3]);
    }

    #[test]
    fn coalescer_settings_and_when_done() {
        let t0 = Instant::now();
        let mut c = Coalescer::new(Duration::from_millis(100));
        let s = Settings::default();
        c.push(Event::SettingsChanged(s.clone()));
        c.push(Event::WhenDoneCountdown {
            action: WhenDone::Sleep,
            seconds_left: 30,
        });
        c.push(Event::WhenDoneCountdown {
            action: WhenDone::Sleep,
            seconds_left: 29,
        });
        let f = c.take_if_due(t0).unwrap();
        assert_eq!(f.settings, Some(s));
        assert_eq!(
            f.when_done,
            Some(WhenDoneUi::Countdown {
                action: WhenDone::Sleep,
                seconds: 29
            })
        );
        c.push(Event::WhenDoneCancelled);
        c.push(Event::WhenDoneFired {
            action: WhenDone::Quit,
            command: None,
        });
        let f = c.take_if_due(t0 + Duration::from_secs(1)).unwrap();
        assert_eq!(
            f.when_done,
            Some(WhenDoneUi::Fired {
                action: WhenDone::Quit
            })
        );
        assert!(f.settings.is_none());
    }
}
