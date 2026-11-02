//! Changing a torrent job after it was added: which of its files it
//! downloads, and its own sharing rules.

use std::collections::HashSet;

use crate::errors::{CoreError, ErrorKind};
use crate::plan::Segment;
use crate::store::{JobId, JobKind, JobRecord, JobState};
use crate::torrent::TorrentInfo;
use crate::torrent::files::{check_selection, is_selected, selected_total};
use crate::torrent::hub::info_from_bytes;
use crate::torrent::policy::TorrentOverrides;

use super::actor::{Actor, Command};
use super::jobs::view_of;
use super::schedule::check_space;
use super::{Engine, Event, no_such_job, read, write};

impl Engine {
    /// Downloads the torrent's files `only_files` (indices into
    /// `TorrentInfo::files`, in any order) from now on, in any state. The
    /// files still to download must fit on the disk beside what other
    /// downloads there still need (else `DiskFull`, `FileTooLargeForDisk`);
    /// an empty selection is refused ("select at least one file"), and so is
    /// an index the torrent does not have. A refused change changes nothing.
    ///
    /// Files that stay selected keep what is downloaded; files left out keep
    /// theirs on disk too (librqbit never deletes them). The job's total
    /// becomes the size of the files selected. A running job (Downloading,
    /// or Seeding) restarts its run with the new selection at once, even if
    /// `torrent.max_active` torrents already download (like `start_now`); a
    /// Seeding one that gains files downloads them before it shares again,
    /// its seeding time counted anew from then (its uploads are kept). A
    /// Completed job that gains files is Queued again; one that only loses
    /// files stays Completed. A Paused or Failed job keeps its state and
    /// takes the selection when it next runs. A magnet link whose metadata
    /// has not come yet only has the selection stored (checked when it
    /// comes).
    pub async fn set_torrent_files(
        &self,
        id: JobId,
        only_files: Vec<usize>,
    ) -> Result<(), CoreError> {
        self.call(|reply| Command::SetTorrentFiles {
            id,
            only_files,
            reply,
        })
        .await?
    }

    /// Sets the torrent's own sharing rules; each one left `None` follows
    /// the global torrent setting. Saved with the job, they apply at once,
    /// also to a torrent that is sharing (one whose new limit is already
    /// reached stops sharing and is Completed). A ratio that is not a
    /// finite number is refused.
    pub async fn set_torrent_overrides(
        &self,
        id: JobId,
        o: TorrentOverrides,
    ) -> Result<(), CoreError> {
        self.call(|reply| Command::SetTorrentOverrides {
            id,
            overrides: o,
            reply,
        })
        .await?
    }
}

impl Actor {
    pub(super) async fn set_torrent_files(
        &mut self,
        id: JobId,
        mut only: Vec<usize>,
    ) -> Result<(), CoreError> {
        let job = self.torrent_job(id)?;
        // `is_selected` needs it sorted, without duplicates.
        only.sort_unstable();
        only.dedup();
        if only.is_empty() {
            return Err(unknown("select at least one file"));
        }
        let info = job
            .torrent_meta
            .as_deref()
            .map(info_from_bytes)
            .transpose()?;
        if let Some(info) = &info {
            check_selection(info, Some(&only))?;
            if selected_set(info, job.only_files.as_deref()) == selected_set(info, Some(&only)) {
                return Ok(());
            }
            self.check_reselection_fits(&job, info, &only)?;
        } else if job.only_files.as_deref() == Some(only.as_slice()) {
            return Ok(());
        }

        let was_running = self.running.get(&id).map(|run| run.cancel.clone());
        if let Some(cancel) = &was_running {
            // The run stops only to start again: its waiters wait on.
            let waiters = self.waiters.remove(&id);
            cancel.cancel();
            self.settle(&[id]).await;
            if let Some(waiters) = waiters {
                self.waiters.entry(id).or_default().extend(waiters);
            }
        }
        // As the stopped run left it.
        let mut job = self.torrent_job(id)?;
        let have = info.as_ref().map(|info| self.have_per_file(&job, info));
        let gained = info.as_ref().is_some_and(|info| {
            info.files.iter().any(|f| {
                is_selected(Some(&only), f.index)
                    && !is_selected(job.only_files.as_deref(), f.index)
            })
        });
        if gained {
            // Not all downloaded any more: it finishes (and is shared) anew.
            job.seed_started_at = None;
        }
        let restart = was_running.is_some() && job.state == JobState::Paused;
        if restart || (gained && job.state == JobState::Completed) {
            job.state = JobState::Queued;
            job.error = None;
        }
        let downloaded = match (&info, &have) {
            (Some(info), Some(have)) => info
                .files
                .iter()
                .filter(|f| is_selected(Some(&only), f.index))
                .map(|f| have[f.index])
                .sum(),
            _ => 0,
        };
        job.total = info.as_ref().map(|info| selected_total(info, Some(&only)));
        job.only_files = Some(only);
        self.store.update_job(&job)?;
        // The runner's one segment, `[0, total)`; it reads the rest from the
        // session as it runs.
        let segments: Vec<Segment> = job
            .total
            .map(|total| Segment {
                start: 0,
                end: total,
                done: downloaded.min(total),
            })
            .into_iter()
            .collect();
        self.store.replace_segments(id, &segments)?;

        let mut view = view_of(&self.store, &job, downloaded);
        view.retry_at = read(&self.views).get(&id).and_then(|v| v.retry_at);
        write(&self.views).insert(id, view.clone());
        self.emit(Event::JobUpdated(view));
        if restart {
            self.start_torrent(id);
        }
        self.after_change();
        Ok(())
    }

    /// The files `only` selects that are not downloaded yet must fit on the
    /// disk beside what other downloads there still need.
    fn check_reselection_fits(
        &self,
        job: &JobRecord,
        info: &TorrentInfo,
        only: &[usize],
    ) -> Result<(), CoreError> {
        let have = self.have_per_file(job, info);
        let selected = || {
            info.files
                .iter()
                .filter(|f| is_selected(Some(only), f.index))
        };
        let missing = selected()
            .map(|f| f.size.saturating_sub(have[f.index]))
            .fold(0u64, u64::saturating_add);
        let largest = selected().map(|f| f.size).max();
        check_space(
            &self.views,
            self.space,
            &job.dest_dir,
            missing,
            largest,
            Some(job.id),
        )
    }

    /// Bytes downloaded of each of the torrent's files, by index: what the
    /// session found, once it has checked them; otherwise all of each file
    /// selected so far if the download finished, and nothing of any other
    /// (which may count too little as downloaded, never too much).
    fn have_per_file(&self, job: &JobRecord, info: &TorrentInfo) -> Vec<u64> {
        let progress = job
            .info_hash
            .as_deref()
            .and_then(|hash| self.hub.file_progress(hash))
            .filter(|p| p.len() == info.files.len());
        let finished = job.seed_started_at.is_some();
        info.files
            .iter()
            .map(|f| match &progress {
                Some(progress) => progress[f.index].min(f.size),
                None if finished && is_selected(job.only_files.as_deref(), f.index) => f.size,
                None => 0,
            })
            .collect()
    }

    pub(super) fn set_torrent_overrides(
        &mut self,
        id: JobId,
        overrides: TorrentOverrides,
    ) -> Result<(), CoreError> {
        self.torrent_job(id)?;
        if overrides
            .stop_ratio
            .flatten()
            .is_some_and(|r| !r.is_finite())
        {
            return Err(unknown("the stop ratio must be a finite number"));
        }
        self.store.set_torrent_overrides(id, &overrides)?;
        if let Some(rules) = self.seed_rules.get(&id) {
            rules.send_replace(overrides.clone());
        }
        let view = write(&self.views).get_mut(&id).and_then(|view| {
            view.torrent.as_mut()?.overrides = overrides;
            Some(view.clone())
        });
        if let Some(view) = view {
            self.emit(Event::JobUpdated(view));
        }
        Ok(())
    }

    fn torrent_job(&self, id: JobId) -> Result<JobRecord, CoreError> {
        let job = self.store.get_job(id)?.ok_or_else(no_such_job)?;
        if job.kind != JobKind::Torrent {
            return Err(unknown("this applies only to torrents"));
        }
        Ok(job)
    }
}

/// The indices `only_files` selects (`None`: every file).
fn selected_set(info: &TorrentInfo, only_files: Option<&[usize]>) -> HashSet<usize> {
    info.files
        .iter()
        .map(|f| f.index)
        .filter(|&i| is_selected(only_files, i))
        .collect()
}

fn unknown(detail: &str) -> CoreError {
    CoreError {
        kind: ErrorKind::Unknown,
        detail: detail.to_string(),
    }
}
