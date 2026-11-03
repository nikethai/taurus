//! Advanced mode's list in the window: the layout (from `ui.mode` and the
//! window width), the rows of the current tab or filter and search, the
//! counts, the footer speeds, the selection and the wide layout's detail
//! pane. Refreshed with the Simple lists, from the app's 100 ms timer.

use std::rc::Rc;

use slint::{ModelRc, VecModel};
use taurus_core::{JobId, JobState, JobView};

use crate::app::App;
use crate::filters::{self, Filter, Tab};
use crate::mode::{self, Layout};
use crate::pump::{set_model, unix_now};
use crate::{AdvRowVm, AppWindow, DetailStubVm, FilterCountVm, TabCountsVm, rows};

pub(crate) struct AdvState {
    rows: Rc<VecModel<AdvRowVm>>,
    /// Always every filter, in `Filter::ALL` order.
    counts: Rc<VecModel<FilterCountVm>>,
    layout: Layout,
    tab: Tab,
    filter: Filter,
    search: String,
    selected: Option<JobId>,
}

impl Default for AdvState {
    fn default() -> Self {
        Self {
            rows: Rc::new(VecModel::default()),
            counts: Rc::new(VecModel::default()),
            layout: Layout::Simple,
            tab: Tab::Active,
            filter: Filter::All,
            search: String::new(),
            selected: None,
        }
    }
}

impl AdvState {
    /// Gives the window this state's row model.
    pub fn install(&self, ui: &AppWindow) {
        ui.set_adv_rows(ModelRc::from(self.rows.clone()));
        ui.set_filter_counts(ModelRc::from(self.counts.clone()));
    }
}

/// The jobs the Advanced list shows: the tab's (compact) or the filter's
/// (wide), whose name contains `search` (ignoring case). Finished jobs
/// newest first, the rest in the order given (the engine's: by id).
pub fn listed<'a>(
    views: &'a [JobView],
    layout: Layout,
    tab: Tab,
    filter: Filter,
    search: &str,
) -> Vec<&'a JobView> {
    let search = search.trim().to_lowercase();
    let mut out: Vec<&JobView> = views
        .iter()
        .filter(|v| match layout {
            Layout::Wide => filters::matches(filter, v),
            _ => filters::tab_of(v) == tab,
        })
        .filter(|v| search.is_empty() || v.file_name.to_lowercase().contains(&search))
        .collect();
    let (mut done, open): (Vec<&JobView>, Vec<&JobView>) =
        out.drain(..).partition(|v| v.state == JobState::Completed);
    rows::newest_finished_first(&mut done);
    out.extend(open);
    out.extend(done);
    out
}

fn tab_counts(views: &[JobView]) -> TabCountsVm {
    let n = |tab| views.iter().filter(|v| filters::tab_of(v) == tab).count() as i32;
    TabCountsVm {
        active: n(Tab::Active),
        waiting: n(Tab::Waiting),
        done: n(Tab::Done),
    }
}

fn to_id(id: JobId) -> i32 {
    id.0.clamp(0, i32::MAX as i64) as i32
}

impl App {
    /// Sets the window's layout from `ui.mode` and its width; entering an
    /// Advanced layout fills its list.
    pub(crate) fn apply_layout(&self) {
        let Some(ui) = self.window.upgrade() else {
            return;
        };
        let layout = mode::layout_for(self.settings.borrow().ui.mode, ui.get_window_width());
        let changed = {
            let mut adv = self.adv.borrow_mut();
            std::mem::replace(&mut adv.layout, layout) != layout
        };
        if changed {
            ui.set_layout(layout.as_str().into());
            self.refresh_adv();
        }
    }

    /// Rebuilds the Advanced list from the latest views (nothing to do in
    /// Simple mode).
    pub(crate) fn refresh_adv(&self) {
        let Some(ui) = self.window.upgrade() else {
            return;
        };
        let view = self.view.borrow();
        let views = view.views();
        let mut adv = self.adv.borrow_mut();
        if adv.layout == Layout::Simple {
            return;
        }
        if adv
            .selected
            .is_some_and(|id| !views.iter().any(|v| v.id == id))
        {
            adv.selected = None;
            ui.set_selected_id(-1);
        }
        let now = unix_now();
        let selected = adv.selected;
        let listed = listed(views, adv.layout, adv.tab, adv.filter, &adv.search);
        set_model(
            &adv.rows,
            listed
                .iter()
                .map(|v| rows::adv_row_of(v, now, Some(v.id) == selected)),
            |r| r.row.id,
        );
        ui.set_tab_counts(tab_counts(views));
        // The same filters in the same order each time: row by row.
        set_model(
            &adv.counts,
            filters::counts(views)
                .into_iter()
                .map(|(f, count)| FilterCountVm {
                    key: f.key().into(),
                    count: count.min(i32::MAX as u32) as i32,
                }),
            |_| 0,
        );
        let (down, up) = rows::speeds(views);
        ui.set_speed_down_text(down.into());
        ui.set_speed_up_text(up.into());
        let detail = selected
            .and_then(|id| views.iter().find(|v| v.id == id))
            .map_or_else(DetailStubVm::default, |v| DetailStubVm {
                visible: true,
                name: v.file_name.as_str().into(),
                kind: rows::row_kind(v).into(),
                source: rows::source_of(v.kind).into(),
            });
        ui.set_detail_stub(detail);
    }

    pub(crate) fn adv_select(&self, id: i32) {
        let selected = self
            .view
            .borrow()
            .views()
            .iter()
            .find(|v| to_id(v.id) == id)
            .map(|v| v.id);
        self.adv.borrow_mut().selected = selected;
        if let Some(ui) = self.window.upgrade() {
            ui.set_selected_id(selected.map_or(-1, to_id));
        }
        self.refresh_adv();
    }

    pub(crate) fn adv_filter(&self, key: &str) {
        let filter = Filter::from_key(key).unwrap_or(Filter::All);
        self.adv.borrow_mut().filter = filter;
        if let Some(ui) = self.window.upgrade() {
            ui.set_filter(filter.key().into());
        }
        self.refresh_adv();
    }

    pub(crate) fn adv_tab(&self, index: i32) {
        self.adv.borrow_mut().tab = Tab::from_index(index);
        self.refresh_adv();
    }

    pub(crate) fn adv_search(&self, text: &str) {
        self.adv.borrow_mut().search = text.to_owned();
        self.refresh_adv();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::view;

    #[test]
    fn listed_by_tab_filter_and_search() {
        let mut old = view(1, "Old.iso", JobState::Completed);
        old.finished_at = Some(10);
        let mut new = view(2, "new.zip", JobState::Completed);
        new.finished_at = Some(20);
        let views = vec![
            old,
            new,
            view(3, "run.iso", JobState::Downloading),
            view(4, "wait.bin", JobState::Paused),
            view(5, "seed.iso", JobState::Seeding),
        ];
        let ids = |v: Vec<&JobView>| v.iter().map(|v| v.id.0).collect::<Vec<_>>();
        let compact =
            |tab, search| ids(listed(&views, Layout::Compact, tab, Filter::Paused, search));
        assert_eq!(compact(Tab::Active, ""), [3, 5]);
        assert_eq!(compact(Tab::Waiting, ""), [4]);
        assert_eq!(compact(Tab::Done, ""), [2, 1]);
        assert_eq!(compact(Tab::Done, " ISO "), [1]);
        let wide = |filter, search| ids(listed(&views, Layout::Wide, Tab::Done, filter, search));
        assert_eq!(wide(Filter::All, ""), [3, 4, 5, 2, 1]);
        assert_eq!(wide(Filter::All, "iso"), [3, 5, 1]);
        assert_eq!(wide(Filter::Downloading, ""), [3, 5]);
        assert_eq!(wide(Filter::Torrents, ""), Vec::<i64>::new());
        let c = tab_counts(&views);
        assert_eq!((c.active, c.waiting, c.done), (2, 1, 2));
    }
}
