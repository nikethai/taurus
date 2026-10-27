//! Crash-safe persistence of jobs and their byte-range segments (SQLite, WAL).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OpenFlags, OptionalExtension, Row, params};

use crate::errors::{CoreError, ErrorKind, classify_io};
use crate::plan::Segment;

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub struct JobId(pub i64);

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum JobState {
    Queued,
    Downloading,
    Paused,
    Completed,
    Failed,
}

#[derive(Clone, Debug, PartialEq)]
pub struct JobRecord {
    pub id: JobId,
    pub url: String,
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
}

pub struct NewJob {
    pub url: String,
    pub dest_dir: PathBuf,
    pub file_name: String,
    pub connections: u32,
}

const SCHEMA_VERSION: i64 = 1;

const SCHEMA_V1: &str = "
CREATE TABLE jobs (
    id            INTEGER PRIMARY KEY,
    url           TEXT NOT NULL,
    final_url     TEXT,
    dest_dir      TEXT NOT NULL,
    file_name     TEXT NOT NULL,
    state         TEXT NOT NULL,
    total         INTEGER,
    resumable     INTEGER NOT NULL DEFAULT 0,
    etag          TEXT,
    last_modified TEXT,
    connections   INTEGER NOT NULL,
    error_kind    TEXT,
    error_detail  TEXT,
    created_at    INTEGER NOT NULL,
    updated_at    INTEGER NOT NULL
);
CREATE TABLE segments (
    job_id INTEGER NOT NULL REFERENCES jobs(id) ON DELETE CASCADE,
    idx    INTEGER NOT NULL,
    start  INTEGER NOT NULL,
    \"end\"  INTEGER, -- NULL: open-ended (u64::MAX)
    done   INTEGER NOT NULL,
    PRIMARY KEY (job_id, idx)
);
CREATE TABLE settings (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
";

const JOB_COLUMNS: &str = "id, url, final_url, dest_dir, file_name, state, total, resumable, \
                           etag, last_modified, connections, error_kind, error_detail";

impl JobState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Downloading => "downloading",
            Self::Paused => "paused",
            Self::Completed => "completed",
            Self::Failed => "failed",
        }
    }

    fn parse(s: &str) -> Option<JobState> {
        Some(match s {
            "queued" => Self::Queued,
            "downloading" => Self::Downloading,
            "paused" => Self::Paused,
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

fn io_err(e: std::io::Error) -> CoreError {
    CoreError {
        kind: classify_io(&e),
        detail: e.to_string(),
    }
}

fn to_i64(v: u64, what: &str) -> Result<i64, CoreError> {
    i64::try_from(v).map_err(|_| CoreError {
        kind: ErrorKind::Unknown,
        detail: format!("{what} {v} does not fit the database"),
    })
}

fn opt_to_i64(v: Option<u64>, what: &str) -> Result<Option<i64>, CoreError> {
    v.map(|v| to_i64(v, what)).transpose()
}

/// Reads an INTEGER column as `u64`; a negative value means a damaged row.
fn col_u64(row: &Row<'_>, idx: usize) -> rusqlite::Result<u64> {
    let v: i64 = row.get(idx)?;
    u64::try_from(v).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(idx, rusqlite::types::Type::Integer, Box::new(e))
    })
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

fn is_corruption(e: &rusqlite::Error) -> bool {
    matches!(
        e.sqlite_error_code(),
        Some(rusqlite::ErrorCode::NotADatabase | rusqlite::ErrorCode::DatabaseCorrupt)
    )
}

fn job_from_row(row: &Row<'_>) -> rusqlite::Result<JobRecord> {
    let state: String = row.get(5)?;
    let state = JobState::parse(&state).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            5,
            rusqlite::types::Type::Text,
            format!("unknown job state {state:?}").into(),
        )
    })?;
    let total: Option<i64> = row.get(6)?;
    let total = total.map(|_| col_u64(row, 6)).transpose()?;
    let connections: i64 = row.get(10)?;
    let connections = u32::try_from(connections).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(10, rusqlite::types::Type::Integer, Box::new(e))
    })?;
    let error_kind: Option<String> = row.get(11)?;
    let error_detail: Option<String> = row.get(12)?;
    let error = error_kind.map(|k| (ErrorKind::from_str(&k), error_detail.unwrap_or_default()));
    Ok(JobRecord {
        id: JobId(row.get(0)?),
        url: row.get(1)?,
        final_url: row.get(2)?,
        dest_dir: PathBuf::from(row.get::<_, String>(3)?),
        file_name: row.get(4)?,
        state,
        total,
        resumable: row.get::<_, i64>(7)? != 0,
        etag: row.get(8)?,
        last_modified: row.get(9)?,
        connections,
        error,
    })
}

/// Opens, verifies and migrates the database at `path`.
fn init(path: &Path) -> rusqlite::Result<Connection> {
    let conn = Connection::open(path)?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;

    let check: String = conn.query_row("PRAGMA quick_check", [], |r| r.get(0))?;
    if check != "ok" {
        return Err(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CORRUPT),
            Some(check),
        ));
    }

    // WAL + NORMAL survives a killed process; only an OS crash can lose the
    // last few commits, which M1's crash model does not cover.
    conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get::<_, String>(0))?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", true)?;

    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version > SCHEMA_VERSION {
        return Err(rusqlite::Error::InvalidParameterName(format!(
            "database schema version {version} is newer than supported version {SCHEMA_VERSION}"
        )));
    }
    if version < 1 {
        conn.execute_batch(&format!(
            "BEGIN; {SCHEMA_V1} PRAGMA user_version = {SCHEMA_VERSION}; COMMIT;"
        ))?;
    }
    Ok(conn)
}

/// Renames the database and its `-wal`/`-shm` siblings to `<name>.corrupt-<secs>[suffix]`.
fn move_aside(path: &Path) -> Result<(), CoreError> {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned());
    let name = name.ok_or_else(|| CoreError {
        kind: ErrorKind::Unknown,
        detail: format!("invalid database path {}", path.display()),
    })?;
    let secs = now_secs();
    for suffix in ["", "-wal", "-shm"] {
        let from = path.with_file_name(format!("{name}{suffix}"));
        let to = path.with_file_name(format!("{name}.corrupt-{secs}{suffix}"));
        match std::fs::rename(&from, &to) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(io_err(e)),
        }
    }
    Ok(())
}

/// Shared handle to the database; clones talk to the same connection.
#[derive(Clone)]
pub struct Store {
    conn: Arc<Mutex<Connection>>,
}

impl Store {
    /// Opens (creating if needed) the database. A file that is not a valid
    /// database is moved aside as `<name>.corrupt-<unix secs>` and replaced
    /// by a fresh one; any other failure is returned.
    pub fn open(path: &Path) -> Result<Store, CoreError> {
        let conn = match init(path) {
            Ok(conn) => conn,
            Err(e) if is_corruption(&e) => {
                move_aside(path)?;
                init(path).map_err(db_err)?
            }
            Err(e) => return Err(db_err(e)),
        };
        Ok(Store {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Opens an existing database for reading only: nothing is created,
    /// migrated or repaired. `None` if there is no database or no schema yet.
    pub fn open_read_only(path: &Path) -> Result<Option<Store>, CoreError> {
        if !path.exists() {
            return Ok(None);
        }
        let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        let conn = Connection::open_with_flags(path, flags).map_err(db_err)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))
            .map_err(db_err)?;
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .map_err(db_err)?;
        if version > SCHEMA_VERSION {
            return Err(CoreError {
                kind: ErrorKind::Unknown,
                detail: format!(
                    "database schema version {version} is newer than supported version \
                     {SCHEMA_VERSION}"
                ),
            });
        }
        if version < 1 {
            return Ok(None);
        }
        Ok(Some(Store {
            conn: Arc::new(Mutex::new(conn)),
        }))
    }

    fn lock(&self) -> MutexGuard<'_, Connection> {
        // A panic while holding the lock leaves at worst a rolled-back transaction.
        self.conn.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn insert_job(&self, job: &NewJob) -> Result<JobId, CoreError> {
        let now = now_secs();
        let conn = self.lock();
        conn.execute(
            "INSERT INTO jobs (url, dest_dir, file_name, state, resumable, connections, \
             created_at, updated_at) VALUES (?1, ?2, ?3, ?4, 0, ?5, ?6, ?6)",
            params![
                job.url,
                job.dest_dir.to_string_lossy(),
                job.file_name,
                JobState::Queued.as_str(),
                job.connections,
                now,
            ],
        )
        .map_err(db_err)?;
        Ok(JobId(conn.last_insert_rowid()))
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

    /// Writes every mutable field of `job`. Updating a job that no longer
    /// exists is a no-op.
    pub fn update_job(&self, job: &JobRecord) -> Result<(), CoreError> {
        let total = opt_to_i64(job.total, "total size")?;
        let (error_kind, error_detail) = match &job.error {
            Some((kind, detail)) => (Some(kind.as_str()), Some(detail.as_str())),
            None => (None, None),
        };
        self.lock()
            .execute(
                "UPDATE jobs SET url = ?2, final_url = ?3, dest_dir = ?4, file_name = ?5, \
                 state = ?6, total = ?7, resumable = ?8, etag = ?9, last_modified = ?10, \
                 connections = ?11, error_kind = ?12, error_detail = ?13, updated_at = ?14 \
                 WHERE id = ?1",
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
                ],
            )
            .map_err(db_err)?;
        Ok(())
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

/// Encodes a segment as `(start, end, done)` column values; an end of
/// `u64::MAX` (open-ended) becomes NULL.
fn encode(seg: &Segment) -> Result<(i64, Option<i64>, i64), CoreError> {
    let end = if seg.end == u64::MAX {
        None
    } else {
        Some(to_i64(seg.end, "segment end")?)
    };
    Ok((
        to_i64(seg.start, "segment start")?,
        end,
        to_i64(seg.done, "segment progress")?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open_temp() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("taurus.db")).unwrap();
        (dir, store)
    }

    fn new_job(name: &str, dest: &str) -> NewJob {
        NewJob {
            url: format!("http://example.test/{name}"),
            dest_dir: PathBuf::from(dest),
            file_name: name.to_string(),
            connections: 8,
        }
    }

    fn seg(start: u64, end: u64, done: u64) -> Segment {
        Segment { start, end, done }
    }

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
    fn open_configures_wal_and_foreign_keys() {
        let (_dir, store) = open_temp();
        let conn = store.conn.lock().unwrap();
        let mode: String = conn
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode, "wal");
        let fk: i64 = conn
            .query_row("PRAGMA foreign_keys", [], |r| r.get(0))
            .unwrap();
        assert_eq!(fk, 1);
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, 1);
    }

    #[test]
    fn clones_share_one_database() {
        let (_dir, store) = open_temp();
        let clone = store.clone();
        let id = store.insert_job(&new_job("a", "/d")).unwrap();
        assert!(clone.get_job(id).unwrap().is_some());
    }

    #[test]
    fn newer_schema_version_is_an_error_not_a_reset() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("taurus.db");
        drop(Store::open(&path).unwrap());
        Connection::open(&path)
            .unwrap()
            .pragma_update(None, "user_version", 99)
            .unwrap();
        assert!(Store::open(&path).is_err());
        let moved = std::fs::read_dir(dir.path())
            .unwrap()
            .any(|e| e.unwrap().file_name().to_string_lossy().contains("corrupt"));
        assert!(!moved);
    }

    #[test]
    fn corrupt_db_is_moved_aside() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("taurus.db");
        std::fs::write(&path, b"not a database").unwrap();
        std::fs::write(dir.path().join("taurus.db-wal"), b"stale wal").unwrap();

        let store = Store::open(&path).unwrap();
        assert!(store.list_jobs().unwrap().is_empty());
        // The fresh database is fully usable.
        let id = store.insert_job(&new_job("a", "/d")).unwrap();
        assert!(store.get_job(id).unwrap().is_some());

        let names: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        let moved = names
            .iter()
            .find(|n| n.starts_with("taurus.db.corrupt-"))
            .unwrap_or_else(|| panic!("no moved-aside db in {names:?}"));
        let secs: u64 = moved["taurus.db.corrupt-".len()..].parse().unwrap();
        assert!(secs > 1_600_000_000);
        assert_eq!(
            std::fs::read(dir.path().join(moved)).unwrap(),
            b"not a database"
        );
    }

    #[test]
    fn move_aside_takes_wal_and_shm_along() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("taurus.db");
        for suffix in ["", "-wal", "-shm"] {
            std::fs::write(dir.path().join(format!("taurus.db{suffix}")), suffix).unwrap();
        }
        move_aside(&path).unwrap();

        let mut names: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names.len(), 3, "{names:?}");
        let base = &names[0];
        assert!(base.starts_with("taurus.db.corrupt-"), "{names:?}");
        assert_eq!(names[1], format!("{base}-shm"));
        assert_eq!(names[2], format!("{base}-wal"));
        // Nothing left to move is not an error.
        move_aside(&path).unwrap();
    }
}
