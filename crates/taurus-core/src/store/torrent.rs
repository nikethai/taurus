//! What only torrent jobs store: their progress and their own sharing
//! rules, each with a writer of its own.

use rusqlite::params;

use crate::errors::{CoreError, ErrorKind};
use crate::plan::Segment;
use crate::torrent::policy::TorrentOverrides;

use super::rows::{encode, only_files_json, opt_to_i64, to_i64};
use super::{JobId, JobRecord, Store, db_err, now_secs};

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

    /// Sets a torrent's file selection as `job` has it, in one transaction:
    /// its `only_files`, `total`, `seed_started_at`, `state` and `error`,
    /// and its segments, replaced by `segments`. Nothing else is written.
    pub fn set_torrent_selection(
        &self,
        job: &JobRecord,
        segments: &[Segment],
    ) -> Result<(), CoreError> {
        let only_files = only_files_json(job.only_files.as_deref())?;
        let total = opt_to_i64(job.total, "total size")?;
        let (error_kind, error_detail) = match &job.error {
            Some((kind, detail)) => (Some(kind.as_str()), Some(detail.as_str())),
            None => (None, None),
        };
        let encoded = segments.iter().map(encode).collect::<Result<Vec<_>, _>>()?;
        let mut conn = self.lock();
        let tx = conn.transaction().map_err(db_err)?;
        tx.execute(
            "UPDATE jobs SET only_files = ?2, total = ?3, seed_started_at = ?4, state = ?5, \
             error_kind = ?6, error_detail = ?7, updated_at = ?8 WHERE id = ?1",
            params![
                job.id.0,
                only_files,
                total,
                job.seed_started_at,
                job.state.as_str(),
                error_kind,
                error_detail,
                now_secs(),
            ],
        )
        .map_err(db_err)?;
        tx.execute("DELETE FROM segments WHERE job_id = ?1", [job.id.0])
            .map_err(db_err)?;
        for (idx, (start, end, done)) in encoded.into_iter().enumerate() {
            tx.execute(
                "INSERT INTO segments (job_id, idx, start, \"end\", done) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![job.id.0, idx as i64, start, end, done],
            )
            .map_err(db_err)?;
        }
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

#[cfg(test)]
mod tests {
    use super::super::test_support::{open_temp, seg, torrent_job};
    use super::super::{JobRecord, JobState};
    use crate::errors::ErrorKind;

    #[test]
    fn selection_is_written_alone_and_whole() {
        let (_dir, store) = open_temp();
        let id = store.insert_job(&torrent_job("t", "abcd")).unwrap();
        store.replace_segments(id, &[seg(0, 100, 40)]).unwrap();
        store.commit_torrent_progress(id, 40, 7).unwrap();
        let before = store.get_job(id).unwrap().unwrap();
        let mut job = JobRecord {
            only_files: Some(vec![0, 1, 2]),
            total: Some(150),
            seed_started_at: None,
            state: JobState::Queued,
            error: None,
            // Not the selection's: left as stored.
            file_name: "other".into(),
            ..before.clone()
        };
        job.error = Some((ErrorKind::Unknown, "x".into()));
        store
            .set_torrent_selection(&job, &[seg(0, 150, 40)])
            .unwrap();
        let stored = store.get_job(id).unwrap().unwrap();
        assert_eq!(
            stored,
            JobRecord {
                file_name: before.file_name.clone(),
                ..job.clone()
            }
        );
        assert_eq!(stored.uploaded, 7);
        assert_eq!(store.load_segments(id).unwrap(), [seg(0, 150, 40)]);

        // A segment that cannot be stored fails before anything is written.
        job.total = Some(10);
        assert!(
            store
                .set_torrent_selection(&job, &[seg(0, u64::MAX - 1, 0)])
                .is_err()
        );
        assert_eq!(store.get_job(id).unwrap().unwrap().total, Some(150));
        assert_eq!(store.load_segments(id).unwrap(), [seg(0, 150, 40)]);
    }

    /// A statement failing inside the transaction, after the job's row was
    /// updated and its segments deleted, undoes both.
    #[test]
    fn a_failure_inside_the_transaction_writes_nothing() {
        let (_dir, store) = open_temp();
        let id = store.insert_job(&torrent_job("t", "abcd")).unwrap();
        store.replace_segments(id, &[seg(0, 100, 40)]).unwrap();
        let before = store.get_job(id).unwrap().unwrap();
        // Refuses the new segment, the last statement of the transaction.
        store
            .lock()
            .execute_batch(
                "CREATE TEMP TRIGGER refuse_segment BEFORE INSERT ON segments \
                 WHEN NEW.start = 7 BEGIN SELECT RAISE(ABORT, 'refused'); END;",
            )
            .unwrap();
        let job = JobRecord {
            only_files: Some(vec![1]),
            total: Some(150),
            state: JobState::Queued,
            ..before.clone()
        };
        let err = store
            .set_torrent_selection(&job, &[seg(7, 150, 40)])
            .unwrap_err();
        assert!(err.detail.contains("refused"), "{err}");
        assert_eq!(store.get_job(id).unwrap().unwrap(), before);
        assert_eq!(store.load_segments(id).unwrap(), [seg(0, 100, 40)]);
    }
}
