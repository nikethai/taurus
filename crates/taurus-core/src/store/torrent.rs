//! What only torrent jobs store: their progress and their own sharing
//! rules, each with a writer of its own.

use rusqlite::params;

use crate::errors::{CoreError, ErrorKind};
use crate::torrent::policy::TorrentOverrides;

use super::rows::to_i64;
use super::{JobId, Store, db_err, now_secs};

impl Store {
    /// Sets only a torrent's progress, in one transaction: the selected
    /// bytes downloaded, as the `done` of its one segment (a torrent job has
    /// one, `[0, selected total)`, once its runner has started; before, only
    /// the uploads are set), and the bytes uploaded in all. So no other field
    /// can be overwritten from a stale copy. Updating a job that no longer
    /// exists is a no-op.
    pub fn commit_torrent_progress(
        &self,
        id: JobId,
        downloaded: u64,
        uploaded: u64,
    ) -> Result<(), CoreError> {
        let downloaded = to_i64(downloaded, "downloaded bytes")?;
        let uploaded = to_i64(uploaded, "uploaded bytes")?;
        let mut conn = self.lock();
        let tx = conn.transaction().map_err(db_err)?;
        tx.execute(
            "UPDATE segments SET done = ?2 WHERE job_id = ?1 AND idx = 0",
            params![id.0, downloaded],
        )
        .map_err(db_err)?;
        tx.execute(
            "UPDATE jobs SET uploaded = ?2, updated_at = ?3 WHERE id = ?1",
            params![id.0, uploaded, now_secs()],
        )
        .map_err(db_err)?;
        tx.commit().map_err(db_err)
    }

    /// Sets only the torrent's own sharing rules; the default (nothing
    /// overridden) is stored as NULL. A job that no longer exists is a
    /// no-op.
    pub fn set_torrent_overrides(
        &self,
        id: JobId,
        overrides: &TorrentOverrides,
    ) -> Result<(), CoreError> {
        let json = if *overrides == TorrentOverrides::default() {
            None
        } else {
            Some(serde_json::to_string(overrides).map_err(|e| CoreError {
                kind: ErrorKind::Unknown,
                detail: e.to_string(),
            })?)
        };
        self.lock()
            .execute(
                "UPDATE jobs SET torrent_overrides = ?2, updated_at = ?3 WHERE id = ?1",
                params![id.0, json, now_secs()],
            )
            .map_err(db_err)?;
        Ok(())
    }
}
