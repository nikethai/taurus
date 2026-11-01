//! Which files go when a job is removed.

use super::JobView;
use crate::store::{JobKind, JobState};

/// What `Engine::remove_with` does with a job's files.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RemoveFiles {
    /// Forget the job, leave every file.
    Keep,
    /// Also delete its files (see `Engine::remove`).
    All,
    /// Delete the files only of an unfinished download, decided by the
    /// engine when it removes the job: a partial file would be left behind
    /// for good, while a finished file is the user's. A torrent's files are
    /// finished once Seeding or Completed, and also when it was paused with
    /// all of its data (after seeding).
    IfUnfinished,
}

impl RemoveFiles {
    /// Whether to delete the files of `view` as it is now.
    pub(super) fn deletes(self, view: Option<&JobView>) -> bool {
        match self {
            RemoveFiles::Keep => false,
            RemoveFiles::All => true,
            RemoveFiles::IfUnfinished => view.is_some_and(unfinished),
        }
    }
}

fn unfinished(view: &JobView) -> bool {
    let open = matches!(
        view.state,
        JobState::Queued
            | JobState::Downloading
            | JobState::Paused
            | JobState::Waiting
            | JobState::Failed
    );
    let complete_torrent =
        view.kind == JobKind::Torrent && view.total.is_some_and(|total| view.downloaded >= total);
    open && !complete_torrent
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::store::JobId;

    fn view(state: JobState, kind: JobKind, downloaded: u64) -> JobView {
        JobView {
            id: JobId(1),
            url: String::new(),
            source_url: String::new(),
            file_name: "a.bin".into(),
            dest_dir: PathBuf::from("/tmp"),
            state,
            total: Some(1000),
            downloaded,
            speed_bps: 0,
            resumable: true,
            error: None,
            kind,
            torrent: None,
            retry_at: None,
            finished_at: None,
            info_hash: None,
        }
    }

    #[test]
    fn if_unfinished_deletes_only_unfinished_files() {
        let deletes = |v: &JobView| RemoveFiles::IfUnfinished.deletes(Some(v));
        for state in [
            JobState::Queued,
            JobState::Downloading,
            JobState::Paused,
            JobState::Waiting,
            JobState::Failed,
        ] {
            assert!(deletes(&view(state, JobKind::Http, 0)), "{state:?}");
        }
        for state in [JobState::Seeding, JobState::Completed] {
            assert!(!deletes(&view(state, JobKind::Http, 0)), "{state:?}");
        }
        // A torrent half downloaded and paused: its pieces go.
        assert!(deletes(&view(JobState::Paused, JobKind::Torrent, 10)));
        // Paused after it finished (and shared): the files are the user's.
        assert!(!deletes(&view(JobState::Paused, JobKind::Torrent, 1000)));
        assert!(!deletes(&view(JobState::Seeding, JobKind::Torrent, 1000)));
    }

    #[test]
    fn keep_and_all_ignore_the_job() {
        assert!(!RemoveFiles::Keep.deletes(None));
        assert!(RemoveFiles::All.deletes(None));
        assert!(!RemoveFiles::IfUnfinished.deletes(None));
    }
}
