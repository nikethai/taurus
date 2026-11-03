//! Changing which files of a torrent in the session are downloaded, and
//! reading how much of each file it has.

use std::collections::HashSet;

use librqbit::TorrentStatsState;

use super::{TorrentHub, lib_error};
use crate::errors::CoreError;
use crate::torrent::TorrentInfo;

impl TorrentHub {
    /// Makes the session's torrent download the files `only_files` selects
    /// (`None`: every file), if the session has it and selects others.
    /// librqbit keeps what is downloaded: data of files that stay selected
    /// is never touched, and files no longer selected keep what they have.
    /// Newly selected files are downloaded once the torrent runs. librqbit
    /// refuses a torrent whose files it is checking: that one is left as it
    /// is (the caller selects again once it has started it).
    pub async fn select_files(
        &self,
        info: &TorrentInfo,
        only_files: Option<&[usize]>,
    ) -> Result<(), CoreError> {
        let (Some(session), Some(handle)) = (self.current(), self.handle(&info.info_hash)) else {
            return Ok(());
        };
        let all = || (0..info.files.len()).collect::<HashSet<usize>>();
        let wanted = only_files.map_or_else(all, |only| only.iter().copied().collect());
        let current = handle
            .only_files()
            .map_or_else(all, |only| only.into_iter().collect());
        if wanted == current
            || matches!(handle.stats().state, TorrentStatsState::Initializing { .. })
        {
            return Ok(());
        }
        session
            .update_only_files(&handle, &wanted)
            .await
            .map_err(lib_error)
    }

    /// Bytes the session has of each of the torrent's files, by index, once
    /// it has checked them; `None` if it has not, or does not have the
    /// torrent (or is not running).
    pub fn file_progress(&self, info_hash: &str) -> Option<Vec<u64>> {
        let stats = self.handle(info_hash)?.stats();
        matches!(
            stats.state,
            TorrentStatsState::Live | TorrentStatsState::Paused
        )
        .then_some(stats.file_progress)
    }
}
