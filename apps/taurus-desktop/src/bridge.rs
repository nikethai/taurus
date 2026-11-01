//! Engine events to UI updates: the `Coalescer` keeps the latest state and
//! hands it out at most once per interval, so a burst of progress events
//! costs the UI one refresh.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};
use taurus_core::tools::{ToolKind, ToolState, ToolStatus};
use taurus_core::{Engine, Event, JobId, JobState, JobView, Settings, WhenDone};
use tokio::sync::broadcast::error::RecvError;

/// Feeds the engine's events into `coalescer` on the runtime until the
/// engine's event channel closes. Subscribes, then seeds the coalescer with
/// the engine's snapshot, so no job is missed; after falling behind the
/// channel (events were dropped) it seeds again from a fresh snapshot and
/// the current settings.
pub fn spawn(
    engine: &Engine,
    rt: &tokio::runtime::Handle,
    coalescer: Arc<Mutex<Coalescer>>,
) -> tokio::task::JoinHandle<()> {
    let mut events = engine.subscribe();
    {
        let mut c = lock(&coalescer);
        c.seed(engine.snapshot());
        c.seed_tools(engine.tools_status());
    }
    let engine = engine.clone();
    rt.spawn(async move {
        loop {
            match events.recv().await {
                Ok(event) => {
                    let mut c = lock(&coalescer);
                    // A recording's elapsed time moves in the engine's view
                    // with its progress, without an event of its own.
                    let recording = match &event {
                        Event::Progress { id, .. } => c.is_recording(*id).then_some(*id),
                        _ => None,
                    };
                    c.push(event);
                    if let Some(id) = recording {
                        c.refresh_media(engine.snapshot().into_iter().find(|v| v.id == id));
                    }
                }
                // Events were dropped: jobs and settings are read again.
                // A dropped when-done event (countdown, fired Notify or
                // Quit) is not recoverable this way: the engine keeps no
                // state to query. With a 1024-event channel drained at
                // once this needs a stalled bridge task, and a countdown
                // sends again every second.
                Err(RecvError::Lagged(_)) => {
                    let mut c = lock(&coalescer);
                    c.seed(engine.snapshot());
                    c.seed_tools(engine.tools_status());
                    c.push(Event::SettingsChanged(engine.settings()));
                }
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

/// What the video tools banner on the Downloads screen shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolsBanner {
    Hidden,
    /// A tool is being fetched; the percent of all the fetches running,
    /// when their sizes are known.
    Installing {
        percent: Option<u8>,
    },
    /// The last attempt for a tool failed and none is being fetched.
    Failed,
}

/// The banner for the tools' latest states.
pub fn tools_banner<'a>(states: impl IntoIterator<Item = &'a ToolState>) -> ToolsBanner {
    let mut installing = false;
    let mut failed = false;
    let (mut done, mut total, mut sized) = (0u64, 0u64, true);
    for state in states {
        match state {
            ToolState::Installing {
                downloaded,
                total: size,
            } => {
                installing = true;
                done = done.saturating_add(*downloaded);
                match size {
                    Some(t) => total = total.saturating_add(*t),
                    None => sized = false,
                }
            }
            ToolState::Failed(_) => failed = true,
            ToolState::Missing | ToolState::Ready => {}
        }
    }
    if installing {
        let percent = (sized && total > 0)
            .then(|| (done.min(total) as f64 * 100.0 / total as f64).floor() as u8);
        ToolsBanner::Installing { percent }
    } else if failed {
        ToolsBanner::Failed
    } else {
        ToolsBanner::Hidden
    }
}

/// One refresh for the UI.
#[derive(Clone, Debug)]
pub struct Flush {
    /// Every known job, in id order (the engine's snapshot order).
    pub views: Vec<JobView>,
    /// Jobs were added or removed, or one changed state, since the last
    /// flush: the tabs need rebuilding. When false only numbers moved, so
    /// tab membership and order stand, but the rows and the footer summary
    /// (its time left follows progress) must still be recomputed.
    /// (Addition to the brief's `Flush`; lets the UI skip the tab split.)
    pub structural: bool,
    /// The newest settings, when they changed since the last flush.
    pub settings: Option<Settings>,
    /// The newest when-done event; the newest one wins within a flush.
    pub when_done: Option<WhenDoneUi>,
    /// The tools banner, when it changed since the last flush.
    pub tools: Option<ToolsBanner>,
}

pub struct Coalescer {
    min_interval: Duration,
    views: BTreeMap<JobId, JobView>,
    dirty: bool,
    structural: bool,
    settings: Option<Settings>,
    when_done: Option<WhenDoneUi>,
    /// Each tool's newest state.
    tools: HashMap<ToolKind, ToolState>,
    /// The banner as last flushed (or about to be).
    banner: ToolsBanner,
    banner_changed: bool,
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
            tools: HashMap::new(),
            banner: ToolsBanner::Hidden,
            banner_changed: false,
            last_flush: None,
        }
    }

    /// Replaces the tools' states with `statuses`, from
    /// `engine.tools_status()`.
    pub fn seed_tools(&mut self, statuses: Vec<ToolStatus>) {
        self.tools = statuses.into_iter().map(|s| (s.kind, s.state)).collect();
        self.update_banner();
    }

    fn update_banner(&mut self) {
        let banner = tools_banner(self.tools.values());
        if banner != self.banner {
            self.banner = banner;
            self.banner_changed = true;
            self.dirty = true;
        }
    }

    /// The job is known and Recording.
    pub fn is_recording(&self, id: JobId) -> bool {
        self.views
            .get(&id)
            .is_some_and(|v| v.state == JobState::Recording)
    }

    /// Takes the media part (elapsed time, phase, entries) of `fresh`, the
    /// engine's current view of a known job.
    pub fn refresh_media(&mut self, fresh: Option<JobView>) {
        let Some(fresh) = fresh else {
            return;
        };
        if let Some(v) = self.views.get_mut(&fresh.id)
            && v.media != fresh.media
        {
            v.media = fresh.media;
            self.dirty = true;
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
            Event::ToolStatus(status) => {
                self.tools.insert(status.kind, status.state);
                return self.update_banner();
            }
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
            tools: std::mem::take(&mut self.banner_changed).then_some(self.banner),
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

    fn tool(kind: ToolKind, state: ToolState) -> Event {
        Event::ToolStatus(ToolStatus {
            kind,
            version: None,
            path: None,
            state,
        })
    }

    #[test]
    fn tools_banner_follows_the_newest_states() {
        let installing = |downloaded, total| ToolState::Installing { downloaded, total };
        assert_eq!(
            tools_banner(&[ToolState::Ready, ToolState::Missing]),
            ToolsBanner::Hidden
        );
        assert_eq!(
            tools_banner(&[installing(42, Some(100)), ToolState::Ready]),
            ToolsBanner::Installing { percent: Some(42) }
        );
        // Two fetches together: 30 + 12 of 50 + 50.
        assert_eq!(
            tools_banner(&[installing(30, Some(50)), installing(12, Some(50))]),
            ToolsBanner::Installing { percent: Some(42) }
        );
        assert_eq!(
            tools_banner(&[installing(30, None), installing(12, Some(50))]),
            ToolsBanner::Installing { percent: None }
        );
        assert_eq!(
            tools_banner(&[installing(0, Some(0))]),
            ToolsBanner::Installing { percent: None }
        );
        let failed = ToolState::Failed(taurus_core::ErrorKind::ConnectionLost);
        // A fetch running wins over an earlier failure.
        assert_eq!(
            tools_banner(&[failed.clone(), installing(1, Some(4))]),
            ToolsBanner::Installing { percent: Some(25) }
        );
        assert_eq!(
            tools_banner(&[failed, ToolState::Ready]),
            ToolsBanner::Failed
        );

        let t0 = Instant::now();
        let mut c = Coalescer::new(Duration::from_millis(100));
        c.push(tool(ToolKind::YtDlp, installing(0, None)));
        c.push(tool(ToolKind::YtDlp, installing(10, Some(100))));
        let f = c.take_if_due(t0).unwrap();
        assert_eq!(f.tools, Some(ToolsBanner::Installing { percent: Some(10) }));
        // The same banner again flushes nothing.
        c.push(tool(ToolKind::YtDlp, installing(10, Some(100))));
        assert!(c.take_if_due(t0 + Duration::from_secs(1)).is_none());
        c.push(tool(
            ToolKind::YtDlp,
            ToolState::Failed(taurus_core::ErrorKind::ConnectionLost),
        ));
        c.push(tool(ToolKind::Deno, installing(5, Some(10))));
        let f = c.take_if_due(t0 + Duration::from_secs(2)).unwrap();
        assert_eq!(f.tools, Some(ToolsBanner::Installing { percent: Some(50) }));
        c.push(tool(ToolKind::Deno, ToolState::Ready));
        let f = c.take_if_due(t0 + Duration::from_secs(3)).unwrap();
        assert_eq!(f.tools, Some(ToolsBanner::Failed));
        c.push(tool(ToolKind::YtDlp, ToolState::Ready));
        let f = c.take_if_due(t0 + Duration::from_secs(4)).unwrap();
        assert_eq!(f.tools, Some(ToolsBanner::Hidden));
        // Other flushes carry no banner.
        c.push(Event::SettingsChanged(Settings::default()));
        assert_eq!(
            c.take_if_due(t0 + Duration::from_secs(5)).unwrap().tools,
            None
        );
    }

    #[test]
    fn recording_views_take_the_engines_media() {
        let t0 = Instant::now();
        let mut c = Coalescer::new(Duration::from_millis(100));
        let mut rec = view(1, "live", JobState::Recording);
        rec.media = Some(taurus_core::MediaView {
            phase: taurus_core::media::MediaPhase::Downloading,
            audio_only: false,
            live: true,
            entries_done: 0,
            entries_total: 1,
            live_elapsed_secs: Some(3),
            starts_at: None,
            thumbnail: None,
            files: Vec::new(),
        });
        c.seed(vec![rec.clone(), view(2, "a", JobState::Downloading)]);
        c.take_if_due(t0).unwrap();
        assert!(c.is_recording(JobId(1)));
        assert!(!c.is_recording(JobId(2)) && !c.is_recording(JobId(3)));
        rec.media.as_mut().unwrap().live_elapsed_secs = Some(9);
        c.refresh_media(Some(rec));
        let f = c.take_if_due(t0 + Duration::from_secs(1)).unwrap();
        assert_eq!(
            f.views[0].media.as_ref().unwrap().live_elapsed_secs,
            Some(9)
        );
        c.refresh_media(None);
        assert!(c.take_if_due(t0 + Duration::from_secs(2)).is_none());
    }
}
