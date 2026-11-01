//! Engine state into the window: list models, footer summary, countdown,
//! settings, and the notifications that state changes call for. Runs from
//! the app's 100 ms timer.

use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use slint::{Model, ModelRc, VecModel};
use taurus_core::{ErrorKind, JobId, JobState, JobView, WhenDone};

use crate::app::App;
use crate::bridge::{Flush, WhenDoneUi};
use crate::shell_text::simple_message;
use crate::{AppWindow, CountdownVm, RowVm, notify, rows};

/// Rows also refresh this often without events: retry countdowns and
/// "finished ... ago" texts move with the clock.
const CLOCK: Duration = Duration::from_secs(1);

/// What the window shows of the jobs, and what was last seen of them.
pub(crate) struct ViewState {
    downloading: Rc<VecModel<RowVm>>,
    finished: Rc<VecModel<RowVm>>,
    views: Vec<JobView>,
    /// State of every job at the last flush, to spot transitions.
    states: HashMap<JobId, JobState>,
    /// A flush arrived: `states` is a baseline for notifications.
    primed: bool,
    refreshed: Option<Instant>,
}

impl Default for ViewState {
    fn default() -> Self {
        Self {
            downloading: Rc::new(VecModel::default()),
            finished: Rc::new(VecModel::default()),
            views: Vec::new(),
            states: HashMap::new(),
            primed: false,
            refreshed: None,
        }
    }
}

impl ViewState {
    /// Gives the window this state's list models.
    pub fn install(&self, ui: &AppWindow) {
        ui.set_downloading_rows(ModelRc::from(self.downloading.clone()));
        ui.set_finished_rows(ModelRc::from(self.finished.clone()));
    }

    pub fn all_paused(&self) -> bool {
        rows::summary(&self.views).all_paused
    }

    pub fn ids_in(&self, states: &[JobState]) -> Vec<JobId> {
        self.views
            .iter()
            .filter(|v| states.contains(&v.state))
            .map(|v| v.id)
            .collect()
    }

    pub fn view(&self, id: JobId) -> Option<&JobView> {
        self.views.iter().find(|v| v.id == id)
    }

    /// Where a job's file is (or will be).
    pub fn path_of(&self, id: JobId) -> Option<PathBuf> {
        self.view(id).map(|v| v.dest_dir.join(&v.file_name))
    }

    /// Rebuilds both tabs and the footer from `views`.
    fn refresh(&mut self, ui: &AppWindow) {
        let now = unix_now();
        let (active, done) = rows::split_tabs(&self.views);
        set_rows(
            &self.downloading,
            active.iter().map(|v| rows::row_of(v, now)),
        );
        set_rows(&self.finished, done.iter().map(|v| rows::row_of(v, now)));
        ui.set_summary(rows::summary(&self.views));
        self.refreshed = Some(Instant::now());
    }
}

/// Puts `rows` in `model`: row by row when the same jobs are listed in the
/// same order (only changed rows are touched), else the whole list at once.
fn set_rows(model: &VecModel<RowVm>, rows: impl Iterator<Item = RowVm>) {
    let rows: Vec<RowVm> = rows.collect();
    let same_ids = model.row_count() == rows.len()
        && model.iter().zip(&rows).all(|(old, new)| old.id == new.id);
    if !same_ids {
        model.set_vec(rows);
        return;
    }
    for (i, row) in rows.into_iter().enumerate() {
        if model.row_data(i).as_ref() != Some(&row) {
            model.set_row_data(i, row);
        }
    }
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// A notification a state change calls for.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Notice {
    Finished(String),
    Failed(String, ErrorKind),
}

/// Jobs that finished downloading (Completed, or Seeding for a torrent) or
/// failed since `old`. A job not in `old` counts as just started.
pub(crate) fn notices(old: &HashMap<JobId, JobState>, views: &[JobView]) -> Vec<Notice> {
    let done = |s: JobState| matches!(s, JobState::Completed | JobState::Seeding);
    views
        .iter()
        .filter_map(|v| {
            let before = old.get(&v.id).copied();
            if done(v.state) && !before.is_some_and(done) {
                Some(Notice::Finished(v.file_name.clone()))
            } else if v.state == JobState::Failed && before != Some(JobState::Failed) {
                let kind = v.error.as_ref().map_or(ErrorKind::Unknown, |(k, _)| *k);
                Some(Notice::Failed(v.file_name.clone(), kind))
            } else {
                None
            }
        })
        .collect()
}

impl App {
    pub(crate) fn apply_flush(&self, flush: Flush) {
        let Some(ui) = self.window.upgrade() else {
            return;
        };
        {
            let mut view = self.view.borrow_mut();
            if view.primed && self.os_shell.get() {
                for notice in notices(&view.states, &flush.views) {
                    match notice {
                        Notice::Finished(name) => notify::finished(&name),
                        Notice::Failed(name, kind) => notify::failed(&name, simple_message(kind)),
                    }
                }
            }
            view.states = flush.views.iter().map(|v| (v.id, v.state)).collect();
            view.primed = true;
            view.views = flush.views;
            view.refresh(&ui);
        }
        if let Some(settings) = flush.settings {
            *self.settings.borrow_mut() = settings;
            self.push_settings();
        }
        if let Some(when_done) = flush.when_done {
            self.when_done(&ui, when_done);
        }
    }

    /// Refreshes the rows once a second when no event did.
    pub(crate) fn refresh_clock(&self) {
        let Some(ui) = self.window.upgrade() else {
            return;
        };
        let mut view = self.view.borrow_mut();
        if view.refreshed.is_some_and(|at| at.elapsed() >= CLOCK) {
            view.refresh(&ui);
        }
    }

    fn when_done(&self, ui: &AppWindow, when_done: WhenDoneUi) {
        match when_done {
            WhenDoneUi::Countdown { action, seconds } => {
                let action = match action {
                    WhenDone::Sleep => "sleep",
                    WhenDone::Shutdown => "shutdown",
                    _ => return self.hide_countdown(),
                };
                ui.set_countdown(CountdownVm {
                    visible: true,
                    action: action.into(),
                    seconds: seconds.min(i32::MAX as u64) as i32,
                });
            }
            WhenDoneUi::Cancelled => self.hide_countdown(),
            WhenDoneUi::Fired { action } => {
                self.hide_countdown();
                match action {
                    WhenDone::Notify if self.os_shell.get() => notify::all_done(),
                    WhenDone::Quit => self.quit(),
                    _ => {}
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::view;

    #[test]
    fn notices_on_transitions_only() {
        let mut failed = view(3, "c.bin", JobState::Failed);
        failed.error = Some((ErrorKind::LinkExpired, String::new()));
        let views = vec![
            view(1, "a.bin", JobState::Completed),
            view(2, "b.iso", JobState::Seeding),
            failed,
            view(4, "d.bin", JobState::Downloading),
            view(5, "new.bin", JobState::Completed),
        ];
        let old = HashMap::from([
            (JobId(1), JobState::Downloading),
            (JobId(2), JobState::Downloading),
            (JobId(3), JobState::Downloading),
            (JobId(4), JobState::Queued),
        ]);
        assert_eq!(
            notices(&old, &views),
            vec![
                Notice::Finished("a.bin".into()),
                Notice::Finished("b.iso".into()),
                Notice::Failed("c.bin".into(), ErrorKind::LinkExpired),
                Notice::Finished("new.bin".into()),
            ]
        );
        // Nothing changed: nothing to say. Seeding to Completed is not news.
        let now: HashMap<JobId, JobState> = views.iter().map(|v| (v.id, v.state)).collect();
        assert!(notices(&now, &views).is_empty());
        let stopped_sharing = vec![view(2, "b.iso", JobState::Completed)];
        assert!(notices(&now, &stopped_sharing).is_empty());
    }
}
