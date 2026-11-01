//! Crash-safe persistence of jobs and their byte-range segments (SQLite, WAL).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, params};

use crate::errors::{CoreError, ErrorKind, classify_io};
use crate::plan::Segment;

mod migrate;
mod rows;
mod settings;
#[cfg(test)]
mod test_support;

use rows::{
    JOB_COLUMNS, col_u64, encode, job_from_row, mirrors_json, only_files_json, opt_to_i64, to_i64,
};

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub struct JobId(pub i64);

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum JobState {
    Queued,
    Downloading,
    Paused,
    Waiting,
    /// A torrent that finished downloading and is sharing.
    Seeding,
    Completed,
    Failed,
}

/// What a job downloads: a file over HTTP, or a torrent.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum JobKind {
    Http,
    Torrent,
}

impl JobKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Torrent => "torrent",
        }
    }

    fn parse(s: &str) -> Option<JobKind> {
        Some(match s {
            "http" => Self::Http,
            "torrent" => Self::Torrent,
            _ => return None,
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct JobRecord {
    pub id: JobId,
    /// The link requested for the file: `source_url` after link fixups and
    /// any confirm form.
    pub url: String,
    /// The link exactly as the user gave it; resolved again on resume.
    pub source_url: String,
    pub final_url: Option<String>,
    pub dest_dir: PathBuf,
    pub file_name: String,
    pub state: JobState,
    pub total: Option<u64>,
    pub resumable: bool,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub connections: u32,
    pub error: Option<(ErrorKind, String)>,
    pub mirrors: Vec<String>,
    pub checksum: Option<String>,
    pub referer: Option<String>,
    pub queue_pos: i64,
    pub kind: JobKind,
    /// Lowercase hex info-hash of a torrent; unique among jobs.
    pub info_hash: Option<String>,
    /// The torrent's metainfo (the bencoded `.torrent` file).
    pub torrent_meta: Option<Vec<u8>>,
    /// Indices of the torrent files to download; `None` is all of them.
    pub only_files: Option<Vec<usize>>,
    /// Bytes uploaded to peers, counted across restarts.
    pub uploaded: u64,
    /// When seeding began (unix secs).
    pub seed_started_at: Option<i64>,
}

pub struct NewJob {
    pub url: String,
    pub source_url: String,
    pub dest_dir: PathBuf,
    pub file_name: String,
    pub connections: u32,
    pub mirrors: Vec<String>,
    pub checksum: Option<String>,
    pub referer: Option<String>,
    /// 0 appends the job to the queue (its position becomes its id).
    pub queue_pos: i64,
    pub kind: JobKind,
    pub info_hash: Option<String>,
    pub torrent_meta: Option<Vec<u8>>,
    pub only_files: Option<Vec<usize>>,
}

impl JobState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Downloading => "downloading",
            Self::Paused => "paused",
            Self::Waiting => "waiting",
            Self::Seeding => "seeding",
            Self::Completed => "completed",
            Self::Failed => "failed",
        }
    }

    fn parse(s: &str) -> Option<JobState> {
        Some(match s {
            "queued" => Self::Queued,
            "downloading" => Self::Downloading,
            "paused" => Self::Paused,
            "waiting" => Self::Waiting,
            "seeding" => Self::Seeding,
            "completed" => Self::Completed,
            "failed" => Self::Failed,
            _ => return None,
        })
    }
}

fn db_err(e: rusqlite::Error) -> CoreError {
    CoreError {
        kind: ErrorKind::Unknown,
        detail: e.to_string(),
    }
}

/// The error for a write that put an info-hash another job already has: the
/// hash's unique index refused it. `conn` is the connection that wrote.
fn duplicate_hash(conn: &Connection, e: rusqlite::Error, info_hash: Option<&str>) -> CoreError {
    let unique = matches!(
        &e,
        rusqlite::Error::SqliteFailure(f, _)
            if f.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE
    );
    let Some(hash) = info_hash.filter(|_| unique) else {
        return db_err(e);
    };
    let existing: rusqlite::Result<i64> =
        conn.query_row("SELECT id FROM jobs WHERE info_hash = ?1", [hash], |r| {
            r.get(0)
        });
    match existing {
        Ok(id) => CoreError {
            kind: ErrorKind::AlreadyAdded,
            detail: format!("this torrent is already added as job {id}"),
        },
        Err(_) => db_err(e),
    }
}

fn io_err(e: std::io::Error) -> CoreError {
    CoreError {
        kind: classify_io(&e),
        detail: e.to_string(),
    }
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// Shared handle to the database; clones talk to the same connection.
#[derive(Clone)]
pub struct Store {
    conn: Arc<Mutex<Connection>>,
}

impl Store {
    fn lock(&self) -> MutexGuard<'_, Connection> {
        // A panic while holding the lock leaves at worst a rolled-back transaction.
        self.conn.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn insert_job(&self, job: &NewJob) -> Result<JobId, CoreError> {
        let now = now_secs();
        let mirrors = mirrors_json(&job.mirrors)?;
        let only_files = only_files_json(job.only_files.as_deref())?;
        let mut conn = self.lock();
        let tx = conn.transaction().map_err(db_err)?;
        tx.execute(
            "INSERT INTO jobs (url, dest_dir, file_name, state, resumable, connections, \
             created_at, updated_at, mirrors, checksum, referer, queue_pos, source_url, \
             kind, info_hash, torrent_meta, only_files) \
             VALUES (?1, ?2, ?3, ?4, 0, ?5, ?6, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
            params![
                job.url,
                job.dest_dir.to_string_lossy(),
                job.file_name,
                JobState::Queued.as_str(),
                job.connections,
                now,
                mirrors,
                job.checksum,
                job.referer,
                job.queue_pos,
                job.source_url,
                job.kind.as_str(),
                job.info_hash,
                job.torrent_meta,
                only_files,
            ],
        )
        .map_err(|e| duplicate_hash(&tx, e, job.info_hash.as_deref()))?;
        let id = tx.last_insert_rowid();
        if job.queue_pos == 0 {
            tx.execute("UPDATE jobs SET queue_pos = id WHERE id = ?1", [id])
                .map_err(db_err)?;
        }
        tx.commit().map_err(db_err)?;
        Ok(JobId(id))
    }

    pub fn get_job(&self, id: JobId) -> Result<Option<JobRecord>, CoreError> {
        self.lock()
            .query_row(
                &format!("SELECT {JOB_COLUMNS} FROM jobs WHERE id = ?1"),
                [id.0],
                job_from_row,
            )
            .optional()
            .map_err(db_err)
    }

    /// When the job was last written (unix secs), if it is Completed: that
    /// is when it finished, as nothing writes a Completed job again.
    pub fn finished_at(&self, id: JobId) -> Result<Option<i64>, CoreError> {
        self.lock()
            .query_row(
                "SELECT updated_at FROM jobs WHERE id = ?1 AND state = ?2",
                params![id.0, JobState::Completed.as_str()],
                |row| row.get(0),
            )
            .optional()
            .map_err(db_err)
    }

    pub fn list_jobs(&self) -> Result<Vec<JobRecord>, CoreError> {
        let conn = self.lock();
        let mut stmt = conn
            .prepare(&format!("SELECT {JOB_COLUMNS} FROM jobs ORDER BY id"))
            .map_err(db_err)?;
        stmt.query_map([], job_from_row)
            .map_err(db_err)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(db_err)
    }

    /// Writes every mutable field of `job` (all but `id`, `source_url`,
    /// `kind` and `uploaded`, which `commit_torrent_progress` owns, so a
    /// stale copy cannot lower it). Updating a job that no longer exists is
    /// a no-op; an info-hash another job has is `AlreadyAdded`.
    pub fn update_job(&self, job: &JobRecord) -> Result<(), CoreError> {
        let total = opt_to_i64(job.total, "total size")?;
        let (error_kind, error_detail) = match &job.error {
            Some((kind, detail)) => (Some(kind.as_str()), Some(detail.as_str())),
            None => (None, None),
        };
        let mirrors = mirrors_json(&job.mirrors)?;
        let only_files = only_files_json(job.only_files.as_deref())?;
        let conn = self.lock();
        conn.execute(
            "UPDATE jobs SET url = ?2, final_url = ?3, dest_dir = ?4, file_name = ?5, \
                 state = ?6, total = ?7, resumable = ?8, etag = ?9, last_modified = ?10, \
                 connections = ?11, error_kind = ?12, error_detail = ?13, updated_at = ?14, \
                 mirrors = ?15, checksum = ?16, referer = ?17, queue_pos = ?18, \
                 info_hash = ?19, torrent_meta = ?20, only_files = ?21, \
                 seed_started_at = ?22 WHERE id = ?1",
            params![
                job.id.0,
                job.url,
                job.final_url,
                job.dest_dir.to_string_lossy(),
                job.file_name,
                job.state.as_str(),
                total,
                job.resumable,
                job.etag,
                job.last_modified,
                job.connections,
                error_kind,
                error_detail,
                now_secs(),
                mirrors,
                job.checksum,
                job.referer,
                job.queue_pos,
                job.info_hash,
                job.torrent_meta,
                only_files,
                job.seed_started_at,
            ],
        )
        .map_err(|e| duplicate_hash(&conn, e, job.info_hash.as_deref()))?;
        Ok(())
    }

    /// The job of the torrent with this info-hash (lowercase hex), if any.
    pub fn find_by_info_hash(&self, info_hash: &str) -> Result<Option<JobId>, CoreError> {
        self.lock()
            .query_row(
                "SELECT id FROM jobs WHERE info_hash = ?1",
                [info_hash],
                |row| Ok(JobId(row.get(0)?)),
            )
            .optional()
            .map_err(db_err)
    }

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

    /// Sets only the job's state, so no other field can be overwritten from
    /// a stale copy. Updating a job that no longer exists is a no-op.
    pub fn set_state(&self, id: JobId, state: JobState) -> Result<(), CoreError> {
        self.lock()
            .execute(
                "UPDATE jobs SET state = ?2, updated_at = ?3 WHERE id = ?1",
                params![id.0, state.as_str(), now_secs()],
            )
            .map_err(db_err)?;
        Ok(())
    }

    /// Puts the job back in the queue (Queued) and clears its error.
    pub fn requeue(&self, id: JobId) -> Result<(), CoreError> {
        self.lock()
            .execute(
                "UPDATE jobs SET state = ?2, error_kind = NULL, error_detail = NULL, \
                 updated_at = ?3 WHERE id = ?1",
                params![id.0, JobState::Queued.as_str(), now_secs()],
            )
            .map_err(db_err)?;
        Ok(())
    }

    /// Puts an interrupted job back in the queue (Queued), marked to be
    /// checked against the server before it runs again (an HTTP job; a
    /// torrent has no server to ask). Its error, place in the queue and
    /// progress are kept.
    pub fn requeue_interrupted(&self, id: JobId) -> Result<(), CoreError> {
        self.lock()
            .execute(
                "UPDATE jobs SET state = ?2, updated_at = ?3, \
                 revalidate = (kind = 'http') WHERE id = ?1",
                params![id.0, JobState::Queued.as_str(), now_secs()],
            )
            .map_err(db_err)?;
        Ok(())
    }

    /// Whether the job must be checked against the server before it runs
    /// (see `requeue_interrupted`). False for a job that does not exist.
    pub fn needs_revalidation(&self, id: JobId) -> Result<bool, CoreError> {
        self.lock()
            .query_row("SELECT revalidate FROM jobs WHERE id = ?1", [id.0], |r| {
                r.get::<_, bool>(0)
            })
            .optional()
            .map(|mark| mark.unwrap_or(false))
            .map_err(db_err)
    }

    /// The job was checked against the server: it runs as it is now.
    pub fn clear_revalidation(&self, id: JobId) -> Result<(), CoreError> {
        self.lock()
            .execute("UPDATE jobs SET revalidate = 0 WHERE id = ?1", [id.0])
            .map_err(db_err)?;
        Ok(())
    }

    /// The Queued jobs in queue order (`queue_pos`, then id).
    pub fn queued_jobs(&self) -> Result<Vec<JobId>, CoreError> {
        let conn = self.lock();
        let mut stmt = conn
            .prepare("SELECT id FROM jobs WHERE state = ?1 ORDER BY queue_pos, id")
            .map_err(db_err)?;
        stmt.query_map([JobState::Queued.as_str()], |row| Ok(JobId(row.get(0)?)))
            .map_err(db_err)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(db_err)
    }

    /// Deletes the job; its segments go with it (`ON DELETE CASCADE`).
    pub fn delete_job(&self, id: JobId) -> Result<(), CoreError> {
        self.lock()
            .execute("DELETE FROM jobs WHERE id = ?1", [id.0])
            .map_err(db_err)?;
        Ok(())
    }

    /// Atomically replaces all segments of the job; `idx` is the slice position.
    pub fn replace_segments(&self, id: JobId, segs: &[Segment]) -> Result<(), CoreError> {
        let mut conn = self.lock();
        let tx = conn.transaction().map_err(db_err)?;
        tx.execute("DELETE FROM segments WHERE job_id = ?1", [id.0])
            .map_err(db_err)?;
        {
            let mut insert = tx
                .prepare(
                    "INSERT INTO segments (job_id, idx, start, \"end\", done) \
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                )
                .map_err(db_err)?;
            for (idx, seg) in segs.iter().enumerate() {
                let (start, end, done) = encode(seg)?;
                insert
                    .execute(params![id.0, idx as i64, start, end, done])
                    .map_err(db_err)?;
            }
        }
        tx.commit().map_err(db_err)
    }

    pub fn load_segments(&self, id: JobId) -> Result<Vec<Segment>, CoreError> {
        let conn = self.lock();
        let mut stmt = conn
            .prepare("SELECT start, \"end\", done FROM segments WHERE job_id = ?1 ORDER BY idx")
            .map_err(db_err)?;
        stmt.query_map([id.0], |row| {
            let end: Option<i64> = row.get(1)?;
            Ok(Segment {
                start: col_u64(row, 0)?,
                end: end.map_or(Ok(u64::MAX), |_| col_u64(row, 1))?,
                done: col_u64(row, 2)?,
            })
        })
        .map_err(db_err)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(db_err)
    }

    /// Persists each segment's `done` and `end` (work stealing lowers ends)
    /// by position, together with the job's `updated_at`, in one transaction.
    /// A segment with no stored row is an error and nothing is written.
    pub fn commit_progress(&self, id: JobId, segs: &[Segment]) -> Result<(), CoreError> {
        let mut conn = self.lock();
        let tx = conn.transaction().map_err(db_err)?;
        {
            let mut update = tx
                .prepare(
                    "UPDATE segments SET done = ?3, \"end\" = ?4 \
                     WHERE job_id = ?1 AND idx = ?2",
                )
                .map_err(db_err)?;
            let mut updated = 0;
            for (idx, seg) in segs.iter().enumerate() {
                let (_, end, done) = encode(seg)?;
                updated += update
                    .execute(params![id.0, idx as i64, done, end])
                    .map_err(db_err)?;
            }
            // Dropping `tx` unchanged rolls the updates back.
            if updated < segs.len() {
                return Err(CoreError {
                    kind: ErrorKind::Unknown,
                    detail: format!(
                        "job {} has {updated} stored segments, progress was for {}",
                        id.0,
                        segs.len()
                    ),
                });
            }
        }
        tx.execute(
            "UPDATE jobs SET updated_at = ?2 WHERE id = ?1",
            params![id.0, now_secs()],
        )
        .map_err(db_err)?;
        tx.commit().map_err(db_err)
    }

    /// Whether a job other than `id` is recorded as `file_name` in `dir`.
    pub fn name_used_by_other(
        &self,
        id: JobId,
        dir: &Path,
        file_name: &str,
    ) -> Result<bool, CoreError> {
        self.lock()
            .query_row(
                "SELECT EXISTS (SELECT 1 FROM jobs \
                 WHERE dest_dir = ?1 AND file_name = ?2 AND id != ?3)",
                params![dir.to_string_lossy(), file_name, id.0],
                |row| row.get(0),
            )
            .map_err(db_err)
    }

    /// File names claimed by unfinished jobs whose destination is `dir`.
    pub fn reserved_names(&self, dir: &Path) -> Result<HashSet<String>, CoreError> {
        let conn = self.lock();
        let mut stmt = conn
            .prepare("SELECT file_name FROM jobs WHERE dest_dir = ?1 AND state != ?2")
            .map_err(db_err)?;
        stmt.query_map(
            params![dir.to_string_lossy(), JobState::Completed.as_str()],
            |row| row.get(0),
        )
        .map_err(db_err)?
        .collect::<rusqlite::Result<HashSet<_>>>()
        .map_err(db_err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_support::*;

    #[test]
    fn job_roundtrip() {
        let (_dir, store) = open_temp();
        let id = store.insert_job(&new_job("a.iso", "/data/dl")).unwrap();

        let job = store.get_job(id).unwrap().unwrap();
        assert_eq!(
            job,
            JobRecord {
                id,
                url: "http://example.test/a.iso".into(),
                source_url: "http://example.test/share/a.iso".into(),
                final_url: None,
                dest_dir: PathBuf::from("/data/dl"),
                file_name: "a.iso".into(),
                state: JobState::Queued,
                total: None,
                resumable: false,
                etag: None,
                last_modified: None,
                connections: 8,
                error: None,
                mirrors: Vec::new(),
                checksum: None,
                referer: None,
                queue_pos: id.0,
                kind: JobKind::Http,
                info_hash: None,
                torrent_meta: None,
                only_files: None,
                uploaded: 0,
                seed_started_at: None,
            }
        );

        let mut updated = job.clone();
        updated.state = JobState::Failed;
        updated.total = Some(5_000_000_000);
        updated.resumable = true;
        updated.etag = Some("\"abc\"".into());
        updated.last_modified = Some("Tue, 01 Jan 2030 00:00:00 GMT".into());
        updated.final_url = Some("http://cdn.test/a.iso".into());
        updated.error = Some((ErrorKind::DiskFull, "no space left".into()));
        store.update_job(&updated).unwrap();
        assert_eq!(store.get_job(id).unwrap().unwrap(), updated);

        updated.state = JobState::Paused;
        updated.error = None;
        store.update_job(&updated).unwrap();
        assert_eq!(store.get_job(id).unwrap().unwrap(), updated);

        assert_eq!(store.get_job(JobId(id.0 + 100)).unwrap(), None);
    }

    #[test]
    fn list_jobs_in_insert_order() {
        let (_dir, store) = open_temp();
        let a = store.insert_job(&new_job("a", "/d")).unwrap();
        let b = store.insert_job(&new_job("b", "/d")).unwrap();
        let ids: Vec<_> = store.list_jobs().unwrap().iter().map(|j| j.id).collect();
        assert_eq!(ids, vec![a, b]);
    }

    #[test]
    fn segments_roundtrip_and_commit() {
        let (_dir, store) = open_temp();
        let id = store.insert_job(&new_job("a", "/d")).unwrap();
        let segs = [seg(0, 100, 0), seg(100, 200, 0), seg(200, 300, 0)];
        store.replace_segments(id, &segs).unwrap();
        assert_eq!(store.load_segments(id).unwrap(), segs);

        // Work stealing lowers an end while progress is committed.
        let progressed = [seg(0, 100, 40), seg(100, 150, 10), seg(200, 300, 99)];
        store.commit_progress(id, &progressed).unwrap();
        assert_eq!(store.load_segments(id).unwrap(), progressed);

        store.replace_segments(id, &[seg(0, 300, 7)]).unwrap();
        assert_eq!(store.load_segments(id).unwrap(), [seg(0, 300, 7)]);
    }

    #[test]
    fn open_ended_segment_roundtrips_as_null() {
        let (_dir, store) = open_temp();
        let id = store.insert_job(&new_job("a", "/d")).unwrap();
        let open_ended = seg(0, u64::MAX, 0);
        store.replace_segments(id, &[open_ended]).unwrap();
        assert_eq!(store.load_segments(id).unwrap(), [open_ended]);

        let progressed = seg(0, u64::MAX, 4096);
        store.commit_progress(id, &[progressed]).unwrap();
        assert_eq!(store.load_segments(id).unwrap(), [progressed]);

        let conn = store.conn.lock().unwrap();
        let end: Option<i64> = conn
            .query_row("SELECT \"end\" FROM segments", [], |r| r.get(0))
            .unwrap();
        assert_eq!(end, None);
    }

    #[test]
    fn values_beyond_i64_are_errors_not_panics() {
        let (_dir, store) = open_temp();
        let id = store.insert_job(&new_job("a", "/d")).unwrap();
        let too_big = u64::MAX - 1;
        assert!(store.replace_segments(id, &[seg(0, too_big, 0)]).is_err());
        assert!(store.replace_segments(id, &[seg(too_big, 1, 0)]).is_err());
        assert!(store.replace_segments(id, &[seg(0, 1, too_big)]).is_err());

        let mut job = store.get_job(id).unwrap().unwrap();
        job.total = Some(u64::MAX);
        let err = store.update_job(&job).unwrap_err();
        assert_eq!(err.kind, ErrorKind::Unknown);
    }

    #[test]
    fn commit_progress_is_atomic() {
        let (_dir, store) = open_temp();
        let id = store.insert_job(&new_job("a", "/d")).unwrap();
        let segs = [seg(0, 100, 0), seg(100, 200, 0)];
        store.replace_segments(id, &segs).unwrap();

        // The second update cannot be encoded, so the first must roll back.
        let bad = [seg(0, 100, 50), seg(100, 200, u64::MAX)];
        assert!(store.commit_progress(id, &bad).is_err());
        assert_eq!(store.load_segments(id).unwrap(), segs);

        // Same for replace_segments: the old rows survive a failed replace.
        assert!(
            store
                .replace_segments(id, &[seg(0, 10, 0), seg(10, 20, u64::MAX)])
                .is_err()
        );
        assert_eq!(store.load_segments(id).unwrap(), segs);
    }

    #[test]
    fn commit_progress_with_unknown_segment_changes_nothing() {
        let (_dir, store) = open_temp();
        let id = store.insert_job(&new_job("a", "/d")).unwrap();
        let segs = [seg(0, 100, 0), seg(100, 200, 0)];
        store.replace_segments(id, &segs).unwrap();

        // A third segment that was never stored would leave a hole.
        let more = [seg(0, 50, 50), seg(100, 200, 10), seg(50, 100, 0)];
        let err = store.commit_progress(id, &more).unwrap_err();
        assert_eq!(err.kind, ErrorKind::Unknown);
        assert_eq!(store.load_segments(id).unwrap(), segs);
    }

    #[test]
    fn commit_progress_refreshes_updated_at() {
        let (_dir, store) = open_temp();
        let id = store.insert_job(&new_job("a", "/d")).unwrap();
        store.replace_segments(id, &[seg(0, 100, 0)]).unwrap();
        let read = |store: &Store| -> i64 {
            let conn = store.conn.lock().unwrap();
            conn.query_row("SELECT updated_at FROM jobs WHERE id = ?1", [id.0], |r| {
                r.get(0)
            })
            .unwrap()
        };
        store
            .conn
            .lock()
            .unwrap()
            .execute("UPDATE jobs SET updated_at = 0", [])
            .unwrap();
        assert_eq!(read(&store), 0);
        store.commit_progress(id, &[seg(0, 100, 10)]).unwrap();
        assert!(read(&store) > 0);

        store
            .conn
            .lock()
            .unwrap()
            .execute("UPDATE jobs SET updated_at = 0", [])
            .unwrap();
        let job = store.get_job(id).unwrap().unwrap();
        store.update_job(&job).unwrap();
        assert!(read(&store) > 0);
    }

    #[test]
    fn delete_cascades() {
        let (_dir, store) = open_temp();
        let id = store.insert_job(&new_job("a", "/d")).unwrap();
        let other = store.insert_job(&new_job("b", "/d")).unwrap();
        store
            .replace_segments(id, &[seg(0, 10, 0), seg(10, 20, 0)])
            .unwrap();
        store.replace_segments(other, &[seg(0, 5, 0)]).unwrap();

        store.delete_job(id).unwrap();
        assert_eq!(store.get_job(id).unwrap(), None);
        assert!(store.load_segments(id).unwrap().is_empty());
        assert_eq!(store.load_segments(other).unwrap().len(), 1);
    }

    #[test]
    fn reserved_names_only_unfinished() {
        let (_dir, store) = open_temp();
        let a = store.insert_job(&new_job("a.iso", "/dl")).unwrap();
        let b = store.insert_job(&new_job("b.iso", "/dl")).unwrap();
        store.insert_job(&new_job("c.iso", "/elsewhere")).unwrap();
        for (id, state) in [(a, JobState::Downloading), (b, JobState::Completed)] {
            let mut job = store.get_job(id).unwrap().unwrap();
            job.state = state;
            store.update_job(&job).unwrap();
        }
        let names = store.reserved_names(Path::new("/dl")).unwrap();
        assert_eq!(names, HashSet::from(["a.iso".to_string()]));
    }

    #[test]
    fn reopen_persists() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("taurus.db");
        let store = Store::open(&path).unwrap();
        let id = store.insert_job(&new_job("a", "/d")).unwrap();
        store
            .replace_segments(id, &[seg(0, 10, 3), seg(10, 20, 0)])
            .unwrap();
        drop(store);

        let store = Store::open(&path).unwrap();
        assert_eq!(store.list_jobs().unwrap().len(), 1);
        assert_eq!(
            store.load_segments(id).unwrap(),
            [seg(0, 10, 3), seg(10, 20, 0)]
        );
    }

    #[test]
    fn torrent_columns_roundtrip() {
        let (_dir, store) = open_temp();
        let id = store.insert_job(&torrent_job("t", "abcd")).unwrap();
        let mut job = store.get_job(id).unwrap().unwrap();
        assert_eq!(job.kind, JobKind::Torrent);
        assert_eq!(job.info_hash.as_deref(), Some("abcd"));
        assert_eq!(job.torrent_meta.as_deref(), Some(&[0, 1, 2, 255][..]));
        assert_eq!(job.only_files, Some(vec![0, 2]));
        assert_eq!((job.uploaded, job.seed_started_at), (0, None));

        job.only_files = None;
        job.torrent_meta = Some(vec![9; 3]);
        job.seed_started_at = Some(1_700_000_000);
        store.update_job(&job).unwrap();
        assert_eq!(store.get_job(id).unwrap().unwrap(), job);

        // Uploaded bytes have their own writer, beyond i64 is refused.
        store.commit_torrent_progress(id, 0, 5_000_000_000).unwrap();
        let stored = store.get_job(id).unwrap().unwrap();
        assert_eq!(stored.uploaded, 5_000_000_000);
        assert_eq!(
            JobRecord {
                uploaded: 0,
                ..stored.clone()
            },
            job
        );
        assert!(store.commit_torrent_progress(id, 0, u64::MAX).is_err());
        // A stale whole-record update leaves them alone.
        store.update_job(&job).unwrap();
        assert_eq!(store.get_job(id).unwrap().unwrap().uploaded, 5_000_000_000);
        // A job that is gone is not an error.
        store.delete_job(id).unwrap();
        store.commit_torrent_progress(id, 1, 1).unwrap();
    }

    #[test]
    fn torrent_progress_is_its_one_segment() {
        let (_dir, store) = open_temp();
        let id = store.insert_job(&torrent_job("t", "abcd")).unwrap();
        // No segment yet: only the uploads are stored.
        store.commit_torrent_progress(id, 10, 3).unwrap();
        assert!(store.load_segments(id).unwrap().is_empty());
        store.replace_segments(id, &[seg(0, 100, 0)]).unwrap();
        store.commit_torrent_progress(id, 40, 7).unwrap();
        assert_eq!(store.load_segments(id).unwrap(), [seg(0, 100, 40)]);
        assert_eq!(store.get_job(id).unwrap().unwrap().uploaded, 7);
        // Both or neither.
        assert!(store.commit_torrent_progress(id, u64::MAX, 9).is_err());
        assert!(store.commit_torrent_progress(id, 50, u64::MAX).is_err());
        assert_eq!(store.load_segments(id).unwrap(), [seg(0, 100, 40)]);
        assert_eq!(store.get_job(id).unwrap().unwrap().uploaded, 7);
    }

    #[test]
    fn seeding_state_roundtrip() {
        let (_dir, store) = open_temp();
        let id = store.insert_job(&torrent_job("t", "abcd")).unwrap();
        store.set_state(id, JobState::Seeding).unwrap();
        assert_eq!(store.get_job(id).unwrap().unwrap().state, JobState::Seeding);
        let mut job = store.get_job(id).unwrap().unwrap();
        job.state = JobState::Completed;
        store.update_job(&job).unwrap();
        job.state = JobState::Seeding;
        store.update_job(&job).unwrap();
        let raw: String = store
            .lock()
            .query_row("SELECT state FROM jobs", [], |r| r.get(0))
            .unwrap();
        assert_eq!(raw, "seeding");
    }

    #[test]
    fn info_hash_is_unique() {
        let (_dir, store) = open_temp();
        let first = store.insert_job(&torrent_job("a", "abcd")).unwrap();
        let err = store.insert_job(&torrent_job("b", "abcd")).unwrap_err();
        assert_eq!(err.kind, ErrorKind::AlreadyAdded);
        assert!(err.detail.contains(&first.0.to_string()), "{}", err.detail);
        // Nothing of the refused job was recorded.
        assert_eq!(store.list_jobs().unwrap().len(), 1);

        // Updating a job to a hash another one has is refused the same way.
        let second = store.insert_job(&torrent_job("c", "ef01")).unwrap();
        let mut job = store.get_job(second).unwrap().unwrap();
        job.info_hash = Some("abcd".into());
        let err = store.update_job(&job).unwrap_err();
        assert_eq!(err.kind, ErrorKind::AlreadyAdded);
        assert!(err.detail.contains(&first.0.to_string()), "{}", err.detail);
        assert_eq!(
            store.get_job(second).unwrap().unwrap().info_hash.as_deref(),
            Some("ef01")
        );
        // Writing a job's own hash again is fine, and HTTP jobs have none.
        job.info_hash = Some("ef01".into());
        store.update_job(&job).unwrap();
        store.insert_job(&new_job("h1", "/d")).unwrap();
        store.insert_job(&new_job("h2", "/d")).unwrap();
    }

    #[test]
    fn find_by_info_hash() {
        let (_dir, store) = open_temp();
        store.insert_job(&new_job("http", "/d")).unwrap();
        let id = store.insert_job(&torrent_job("t", "abcd")).unwrap();
        assert_eq!(store.find_by_info_hash("abcd").unwrap(), Some(id));
        assert_eq!(store.find_by_info_hash("ffff").unwrap(), None);
        store.delete_job(id).unwrap();
        assert_eq!(store.find_by_info_hash("abcd").unwrap(), None);
    }

    #[test]
    fn only_torrents_skip_the_revalidation_mark() {
        let (_dir, store) = open_temp();
        let http = store.insert_job(&new_job("a", "/d")).unwrap();
        let torrent = store.insert_job(&torrent_job("t", "abcd")).unwrap();
        store.set_state(http, JobState::Downloading).unwrap();
        store.set_state(torrent, JobState::Seeding).unwrap();
        store.requeue_interrupted(http).unwrap();
        store.requeue_interrupted(torrent).unwrap();
        assert!(store.needs_revalidation(http).unwrap());
        assert!(!store.needs_revalidation(torrent).unwrap());
        assert_eq!(
            store.get_job(torrent).unwrap().unwrap().state,
            JobState::Queued
        );
    }

    #[test]
    fn interrupted_jobs_are_marked_until_cleared() {
        let (_dir, store) = open_temp();
        let id = store.insert_job(&new_job("a", "/d")).unwrap();
        let mut job = store.get_job(id).unwrap().unwrap();
        job.state = JobState::Downloading;
        job.error = Some((ErrorKind::FileChangedOnServer, "noted".into()));
        job.queue_pos = 9;
        store.update_job(&job).unwrap();
        assert!(!store.needs_revalidation(id).unwrap());

        store.requeue_interrupted(id).unwrap();
        let requeued = store.get_job(id).unwrap().unwrap();
        assert_eq!(requeued.state, JobState::Queued);
        assert_eq!(requeued.error, job.error);
        assert_eq!(requeued.queue_pos, 9);
        assert!(store.needs_revalidation(id).unwrap());
        // A whole-record update leaves the mark alone.
        store.update_job(&requeued).unwrap();
        assert!(store.needs_revalidation(id).unwrap());

        store.clear_revalidation(id).unwrap();
        assert!(!store.needs_revalidation(id).unwrap());
        assert!(!store.needs_revalidation(JobId(999)).unwrap());
    }

    #[test]
    fn waiting_state_roundtrip() {
        let (_dir, store) = open_temp();
        let id = store.insert_job(&new_job("a", "/d")).unwrap();
        let mut job = store.get_job(id).unwrap().unwrap();
        job.state = JobState::Waiting;
        store.update_job(&job).unwrap();
        assert_eq!(store.get_job(id).unwrap().unwrap().state, JobState::Waiting);
        let conn = store.conn.lock().unwrap();
        let raw: String = conn
            .query_row("SELECT state FROM jobs", [], |r| r.get(0))
            .unwrap();
        assert_eq!(raw, "waiting");
    }

    #[test]
    fn state_updates_touch_only_state_and_error() {
        let (_dir, store) = open_temp();
        let late = store
            .insert_job(&NewJob {
                queue_pos: 100,
                ..new_job("late", "/d")
            })
            .unwrap();
        let a = store.insert_job(&new_job("a", "/d")).unwrap();
        let b = store.insert_job(&new_job("b", "/d")).unwrap();
        assert_eq!(store.queued_jobs().unwrap(), [a, b, late]);

        let mut job = store.get_job(a).unwrap().unwrap();
        job.error = Some((ErrorKind::RateLimited, "503".into()));
        job.total = Some(42);
        store.update_job(&job).unwrap();
        store.set_state(a, JobState::Waiting).unwrap();
        job.state = JobState::Waiting;
        assert_eq!(store.get_job(a).unwrap().unwrap(), job);
        assert_eq!(store.queued_jobs().unwrap(), [b, late]);

        store.requeue(a).unwrap();
        job.state = JobState::Queued;
        job.error = None;
        assert_eq!(store.get_job(a).unwrap().unwrap(), job);
        assert_eq!(store.queued_jobs().unwrap(), [a, b, late]);
        // A job that is gone is not an error.
        store.delete_job(b).unwrap();
        store.set_state(b, JobState::Paused).unwrap();
        store.requeue(b).unwrap();
    }

    #[test]
    fn job_extras_roundtrip() {
        let (_dir, store) = open_temp();
        let id = store
            .insert_job(&NewJob {
                mirrors: vec!["http://m1.test/a".into(), "http://m2.test/a".into()],
                checksum: Some("sha256:abcd".into()),
                referer: Some("http://page.test/".into()),
                queue_pos: 50,
                ..new_job("a", "/d")
            })
            .unwrap();
        let mut job = store.get_job(id).unwrap().unwrap();
        assert_eq!(job.mirrors, ["http://m1.test/a", "http://m2.test/a"]);
        assert_eq!(job.checksum.as_deref(), Some("sha256:abcd"));
        assert_eq!(job.referer.as_deref(), Some("http://page.test/"));
        assert_eq!(job.queue_pos, 50);

        job.mirrors = vec!["http://m3.test/a".into()];
        job.checksum = None;
        job.referer = None;
        job.queue_pos = 9;
        store.update_job(&job).unwrap();
        assert_eq!(store.get_job(id).unwrap().unwrap(), job);

        // An explicit position of 0 means "append": it defaults to the new id.
        let b = store.insert_job(&new_job("b", "/d")).unwrap();
        assert_eq!(store.get_job(b).unwrap().unwrap().queue_pos, b.0);
    }

    #[test]
    fn clones_share_one_database() {
        let (_dir, store) = open_temp();
        let clone = store.clone();
        let id = store.insert_job(&new_job("a", "/d")).unwrap();
        assert!(clone.get_job(id).unwrap().is_some());
    }
}
