//! Advanced mode's list filters: the wide layout's sidebar (Library and
//! Sources) and the compact layout's Active / Waiting / Done tabs. Pure.

use taurus_core::{JobKind, JobState, JobView};

/// A sidebar entry. Library: All to Failed; Sources: Http to Media.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Filter {
    All,
    Downloading,
    Queued,
    Paused,
    Completed,
    Failed,
    Http,
    Torrents,
    Media,
}

impl Filter {
    /// Every filter, in sidebar order (the order of `counts`).
    pub const ALL: [Filter; 9] = [
        Filter::All,
        Filter::Downloading,
        Filter::Queued,
        Filter::Paused,
        Filter::Completed,
        Filter::Failed,
        Filter::Http,
        Filter::Torrents,
        Filter::Media,
    ];

    /// The window's `filter` value and `FilterCountVm.key`.
    pub fn key(self) -> &'static str {
        match self {
            Filter::All => "all",
            Filter::Downloading => "downloading",
            Filter::Queued => "queued",
            Filter::Paused => "paused",
            Filter::Completed => "completed",
            Filter::Failed => "failed",
            Filter::Http => "http",
            Filter::Torrents => "torrents",
            Filter::Media => "media",
        }
    }

    pub fn from_key(key: &str) -> Option<Filter> {
        Filter::ALL.into_iter().find(|f| f.key() == key)
    }
}

/// Downloading includes Recording and Seeding; Queued includes Waiting.
/// Media covers yt-dlp downloads and conversions.
pub fn matches(f: Filter, v: &JobView) -> bool {
    use JobState as S;
    match f {
        Filter::All => true,
        Filter::Downloading => matches!(v.state, S::Downloading | S::Recording | S::Seeding),
        Filter::Queued => matches!(v.state, S::Queued | S::Waiting),
        Filter::Paused => v.state == S::Paused,
        Filter::Completed => v.state == S::Completed,
        Filter::Failed => v.state == S::Failed,
        Filter::Http => v.kind == JobKind::Http,
        Filter::Torrents => v.kind == JobKind::Torrent,
        Filter::Media => matches!(v.kind, JobKind::Media | JobKind::Convert),
    }
}

/// How many of `views` each filter lists, for every filter in
/// `Filter::ALL` order.
pub fn counts(views: &[JobView]) -> Vec<(Filter, u32)> {
    Filter::ALL
        .into_iter()
        .map(|f| (f, views.iter().filter(|v| matches(f, v)).count() as u32))
        .collect()
}

/// A compact-layout tab.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tab {
    Active,
    Waiting,
    Done,
}

impl Tab {
    /// The window's tab index: 0 Active, 1 Waiting, 2 Done; anything else
    /// is Active.
    pub fn from_index(i: i32) -> Tab {
        match i {
            1 => Tab::Waiting,
            2 => Tab::Done,
            _ => Tab::Active,
        }
    }
}

/// Active: Downloading, Recording, Seeding. Waiting: Queued, Waiting,
/// Paused, Failed. Done: Completed.
pub fn tab_of(v: &JobView) -> Tab {
    use JobState as S;
    match v.state {
        S::Downloading | S::Recording | S::Seeding => Tab::Active,
        S::Queued | S::Waiting | S::Paused | S::Failed => Tab::Waiting,
        S::Completed => Tab::Done,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::view;

    const STATES: [JobState; 8] = [
        JobState::Queued,
        JobState::Downloading,
        JobState::Paused,
        JobState::Waiting,
        JobState::Seeding,
        JobState::Failed,
        JobState::Completed,
        JobState::Recording,
    ];

    #[test]
    fn filter_and_tab_rules() {
        use Filter as F;
        for state in STATES {
            let v = view(1, "a.bin", state);
            let listed: Vec<Filter> = [
                F::Downloading,
                F::Queued,
                F::Paused,
                F::Completed,
                F::Failed,
            ]
            .into_iter()
            .filter(|f| matches(*f, &v))
            .collect();
            let (want, tab) = match state {
                JobState::Downloading | JobState::Recording | JobState::Seeding => {
                    (F::Downloading, Tab::Active)
                }
                JobState::Queued | JobState::Waiting => (F::Queued, Tab::Waiting),
                JobState::Paused => (F::Paused, Tab::Waiting),
                JobState::Failed => (F::Failed, Tab::Waiting),
                JobState::Completed => (F::Completed, Tab::Done),
            };
            // Each state is in exactly one Library entry (besides All).
            assert_eq!(listed, [want], "{state:?}");
            assert!(matches(F::All, &v), "{state:?}");
            assert_eq!(tab_of(&v), tab, "{state:?}");
        }
        for (kind, want) in [
            (JobKind::Http, F::Http),
            (JobKind::Torrent, F::Torrents),
            (JobKind::Media, F::Media),
            (JobKind::Convert, F::Media),
        ] {
            let v = JobView {
                kind,
                ..view(1, "a", JobState::Queued)
            };
            let sources: Vec<Filter> = [F::Http, F::Torrents, F::Media]
                .into_iter()
                .filter(|f| matches(*f, &v))
                .collect();
            assert_eq!(sources, [want], "{kind:?}");
        }
        for f in Filter::ALL {
            assert_eq!(Filter::from_key(f.key()), Some(f));
        }
        assert_eq!(Filter::from_key("nope"), None);
        assert_eq!(
            [0, 1, 2, 7, -1].map(Tab::from_index),
            [
                Tab::Active,
                Tab::Waiting,
                Tab::Done,
                Tab::Active,
                Tab::Active
            ]
        );
    }

    #[test]
    fn counts_cover_all_filters() {
        let mut views: Vec<JobView> = STATES
            .iter()
            .enumerate()
            .map(|(i, s)| view(i as i64, "a", *s))
            .collect();
        views[0].kind = JobKind::Torrent;
        views[1].kind = JobKind::Media;
        views[2].kind = JobKind::Convert;
        let got = counts(&views);
        assert_eq!(
            got.iter().map(|(f, _)| *f).collect::<Vec<_>>(),
            Filter::ALL,
            "every filter, in sidebar order"
        );
        let n = |f: Filter| got.iter().find(|(g, _)| *g == f).unwrap().1;
        assert_eq!(n(Filter::All), 8);
        assert_eq!(n(Filter::Downloading), 3);
        assert_eq!(n(Filter::Queued), 2);
        assert_eq!(
            (n(Filter::Paused), n(Filter::Completed), n(Filter::Failed)),
            (1, 1, 1)
        );
        assert_eq!(
            (n(Filter::Http), n(Filter::Torrents), n(Filter::Media)),
            (5, 1, 2)
        );
        assert!(counts(&[]).iter().all(|(_, c)| *c == 0));
        assert_eq!(counts(&[]).len(), 9);
    }
}
