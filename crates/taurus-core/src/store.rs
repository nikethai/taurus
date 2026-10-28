//! Crash-safe persistence of jobs and their byte-range segments (SQLite, WAL).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OpenFlags, OptionalExtension, Row, params};

use crate::errors::{CoreError, ErrorKind, classify_io};
use crate::plan::Segment;
use crate::settings::Settings;

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
    pub mirrors: Vec<String>,
    pub checksum: Option<String>,
    pub referer: Option<String>,
    pub queue_pos: i64,
}

pub struct NewJob {
    pub url: String,
    pub dest_dir: PathBuf,
    pub file_name: String,
    pub connections: u32,
    pub mirrors: Vec<String>,
    pub checksum: Option<String>,
    pub referer: Option<String>,
    /// 0 appends the job to the queue (its position becomes its id).
    pub queue_pos: i64,
}

const SCHEMA_VERSION: i64 = 2;

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

/// Applied on top of `SCHEMA_V1`, to fresh and existing v1 databases alike.
const MIGRATE_V1_TO_V2: &str = "
ALTER TABLE jobs ADD COLUMN mirrors TEXT NOT NULL DEFAULT '[]';
ALTER TABLE jobs ADD COLUMN checksum TEXT;
ALTER TABLE jobs ADD COLUMN referer TEXT;
ALTER TABLE jobs ADD COLUMN queue_pos INTEGER NOT NULL DEFAULT 0;
UPDATE jobs SET queue_pos = id;
CREATE TABLE host_flags (
    host            TEXT PRIMARY KEY,
    max_connections INTEGER NOT NULL,
    updated_at      INTEGER NOT NULL
);
";

/// A learned connection limit older than this is forgotten: hosts change.
const HOST_CAP_MAX_AGE_SECS: i64 = 7 * 24 * 60 * 60;

const SETTINGS_KEY: &str = "settings";
/// Where `load_settings_lenient` keeps saved settings it could not read.
const UNREADABLE_SETTINGS_KEY: &str = "settings.unreadable";
const MEASURED_PEAK_KEY: &str = "measured_peak";

const JOB_COLUMNS: &str = "id, url, final_url, dest_dir, file_name, state, total, resumable, \
                           etag, last_modified, connections, error_kind, error_detail, \
                           mirrors, checksum, referer, queue_pos";

impl JobState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Downloading => "downloading",
            Self::Paused => "paused",
            Self::Waiting => "waiting",
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
    let mirrors: String = row.get(13)?;
    let mirrors = serde_json::from_str(&mirrors).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(13, rusqlite::types::Type::Text, Box::new(e))
    })?;
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
        mirrors,
        checksum: row.get(14)?,
        referer: row.get(15)?,
        queue_pos: row.get(16)?,
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
    // Each upgrade is one transaction; a failure leaves the old version intact.
    if version < 1 {
        conn.execute_batch(&format!(
            "BEGIN; {SCHEMA_V1} {MIGRATE_V1_TO_V2} PRAGMA user_version = {SCHEMA_VERSION}; COMMIT;"
        ))?;
    } else if version < 2 {
        conn.execute_batch(&format!(
            "BEGIN; {MIGRATE_V1_TO_V2} PRAGMA user_version = {SCHEMA_VERSION}; COMMIT;"
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
        // Only the instance holding the data dir may migrate it.
        if version < SCHEMA_VERSION {
            return Err(CoreError {
                kind: ErrorKind::Unknown,
                detail: format!(
                    "database schema version {version} is older than {SCHEMA_VERSION}; \
                     start taurus once (with no other instance running) to upgrade it"
                ),
            });
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
        let mirrors = mirrors_json(&job.mirrors)?;
        let mut conn = self.lock();
        let tx = conn.transaction().map_err(db_err)?;
        tx.execute(
            "INSERT INTO jobs (url, dest_dir, file_name, state, resumable, connections, \
             created_at, updated_at, mirrors, checksum, referer, queue_pos) \
             VALUES (?1, ?2, ?3, ?4, 0, ?5, ?6, ?6, ?7, ?8, ?9, ?10)",
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
            ],
        )
        .map_err(db_err)?;
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
        let mirrors = mirrors_json(&job.mirrors)?;
        self.lock()
            .execute(
                "UPDATE jobs SET url = ?2, final_url = ?3, dest_dir = ?4, file_name = ?5, \
                 state = ?6, total = ?7, resumable = ?8, etag = ?9, last_modified = ?10, \
                 connections = ?11, error_kind = ?12, error_detail = ?13, updated_at = ?14, \
                 mirrors = ?15, checksum = ?16, referer = ?17, queue_pos = ?18 \
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
                    mirrors,
                    job.checksum,
                    job.referer,
                    job.queue_pos,
                ],
            )
            .map_err(db_err)?;
        Ok(())
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

    /// The saved settings, or the defaults when none were saved. Unreadable
    /// saved settings are an error, never silently reset.
    pub fn load_settings(&self) -> Result<Settings, CoreError> {
        match self.setting(SETTINGS_KEY)? {
            None => Ok(Settings::default()),
            Some(json) => serde_json::from_str(&json).map_err(|e| CoreError {
                kind: ErrorKind::Unknown,
                detail: format!("saved settings are unreadable: {e}"),
            }),
        }
    }

    /// The saved settings, never failing on their content: unreadable ones
    /// are logged and copied to `settings.unreadable` (replacing an older
    /// copy), then each readable field is kept and the rest defaulted
    /// (everything, if the value is not a JSON object). `max_active` is at
    /// least 1. Only a database error is returned.
    pub fn load_settings_lenient(&self) -> Result<Settings, CoreError> {
        let Some(raw) = self.setting(SETTINGS_KEY)? else {
            return Ok(Settings::default());
        };
        let mut settings = match serde_json::from_str(&raw) {
            Ok(settings) => settings,
            Err(e) => {
                tracing::warn!("saved settings are unreadable ({e}); keeping what can be read");
                self.put_setting(UNREADABLE_SETTINGS_KEY, &raw)?;
                readable_fields(&raw)
            }
        };
        settings.max_active = settings.max_active.max(1);
        Ok(settings)
    }

    pub fn save_settings(&self, s: &Settings) -> Result<(), CoreError> {
        let json = serde_json::to_string(s).map_err(|e| CoreError {
            kind: ErrorKind::Unknown,
            detail: e.to_string(),
        })?;
        self.put_setting(SETTINGS_KEY, &json)
    }

    /// The best download speed seen so far (bytes/s) and when (unix secs).
    pub fn measured_peak(&self) -> Result<Option<(u64, i64)>, CoreError> {
        let Some(raw) = self.setting(MEASURED_PEAK_KEY)? else {
            return Ok(None);
        };
        let parsed = raw
            .split_once(',')
            .and_then(|(bps, at)| Some((bps.parse().ok()?, at.parse().ok()?)));
        parsed.map(Some).ok_or_else(|| CoreError {
            kind: ErrorKind::Unknown,
            detail: format!("saved measured peak {raw:?} is unreadable"),
        })
    }

    pub fn save_measured_peak(&self, bps: u64, at: i64) -> Result<(), CoreError> {
        self.put_setting(MEASURED_PEAK_KEY, &format!("{bps},{at}"))
    }

    /// The learned connection limit of `host` (`host[:port]`), if one was
    /// learned in the last 7 days.
    pub fn host_cap(&self, host: &str) -> Result<Option<u32>, CoreError> {
        let cap: Option<i64> = self
            .lock()
            .query_row(
                "SELECT max_connections FROM host_flags WHERE host = ?1 AND updated_at >= ?2",
                params![host, now_secs().saturating_sub(HOST_CAP_MAX_AGE_SECS)],
                |row| row.get(0),
            )
            .optional()
            .map_err(db_err)?;
        cap.map(|c| {
            u32::try_from(c).map_err(|_| CoreError {
                kind: ErrorKind::Unknown,
                detail: format!("connection limit {c} of {host} is damaged"),
            })
        })
        .transpose()
    }

    /// Records `cap` for `host` unless a lower one was learned in the last 7
    /// days, in one statement, so concurrent saves never raise it; refreshes
    /// its age.
    pub fn set_host_cap(&self, host: &str, cap: u32) -> Result<(), CoreError> {
        self.set_host_cap_at(host, cap, now_secs())
    }

    fn set_host_cap_at(&self, host: &str, cap: u32, at: i64) -> Result<(), CoreError> {
        self.lock()
            .execute(
                "INSERT INTO host_flags (host, max_connections, updated_at) \
                 VALUES (?1, ?2, ?3) ON CONFLICT(host) DO UPDATE SET \
                 max_connections = CASE WHEN updated_at < ?4 THEN excluded.max_connections \
                 ELSE MIN(max_connections, excluded.max_connections) END, \
                 updated_at = excluded.updated_at",
                params![host, cap, at, at.saturating_sub(HOST_CAP_MAX_AGE_SECS)],
            )
            .map_err(db_err)?;
        Ok(())
    }

    fn setting(&self, key: &str) -> Result<Option<String>, CoreError> {
        self.lock()
            .query_row("SELECT value FROM settings WHERE key = ?1", [key], |row| {
                row.get(0)
            })
            .optional()
            .map_err(db_err)
    }

    fn put_setting(&self, key: &str, value: &str) -> Result<(), CoreError> {
        self.lock()
            .execute(
                "INSERT INTO settings (key, value) VALUES (?1, ?2) \
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![key, value],
            )
            .map_err(db_err)?;
        Ok(())
    }
}

/// The defaults, with each field of the JSON object `raw` that reads on its
/// own put in place of its default.
fn readable_fields(raw: &str) -> Settings {
    let Ok(serde_json::Value::Object(saved)) = serde_json::from_str(raw) else {
        return Settings::default();
    };
    let Ok(serde_json::Value::Object(mut fields)) = serde_json::to_value(Settings::default())
    else {
        return Settings::default();
    };
    for (key, value) in saved {
        if !fields.contains_key(&key) {
            continue;
        }
        let mut tried = fields.clone();
        tried.insert(key.clone(), value.clone());
        if serde_json::from_value::<Settings>(serde_json::Value::Object(tried)).is_ok() {
            fields.insert(key, value);
        }
    }
    serde_json::from_value(serde_json::Value::Object(fields)).unwrap_or_default()
}

fn mirrors_json(mirrors: &[String]) -> Result<String, CoreError> {
    serde_json::to_string(mirrors).map_err(|e| CoreError {
        kind: ErrorKind::Unknown,
        detail: e.to_string(),
    })
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
    use crate::settings::{SpeedLimit, WhenDone};

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
            mirrors: Vec::new(),
            checksum: None,
            referer: None,
            queue_pos: 0,
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
                mirrors: Vec::new(),
                checksum: None,
                referer: None,
                queue_pos: id.0,
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
        assert_eq!(version, 2);
    }

    #[test]
    fn v1_database_migrates_to_v2() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("taurus.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(SCHEMA_V1).unwrap();
            conn.pragma_update(None, "user_version", 1).unwrap();
            for name in ["a", "b"] {
                conn.execute(
                    "INSERT INTO jobs (url, dest_dir, file_name, state, connections, \
                     created_at, updated_at) VALUES (?1, '/d', ?1, 'paused', 4, 1, 1)",
                    [name],
                )
                .unwrap();
            }
            conn.execute(
                "INSERT INTO settings (key, value) VALUES ('other', 'kept')",
                [],
            )
            .unwrap();
        }

        let store = Store::open(&path).unwrap();
        let jobs = store.list_jobs().unwrap();
        assert_eq!(jobs.len(), 2);
        for job in &jobs {
            assert_eq!(job.mirrors, Vec::<String>::new());
            assert_eq!(job.queue_pos, job.id.0);
            assert_eq!((job.checksum.clone(), job.referer.clone()), (None, None));
            assert_eq!(job.state, JobState::Paused);
        }
        {
            let conn = store.conn.lock().unwrap();
            let version: i64 = conn
                .query_row("PRAGMA user_version", [], |r| r.get(0))
                .unwrap();
            assert_eq!(version, 2);
            let other: String = conn
                .query_row("SELECT value FROM settings WHERE key = 'other'", [], |r| {
                    r.get(0)
                })
                .unwrap();
            assert_eq!(other, "kept");
        }
        assert_eq!(store.host_cap("h").unwrap(), None);
        // Reopening a migrated database is a no-op.
        drop(store);
        assert_eq!(Store::open(&path).unwrap().list_jobs().unwrap().len(), 2);
    }

    #[test]
    fn read_only_open_refuses_a_v1_database() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("taurus.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(SCHEMA_V1).unwrap();
            conn.pragma_update(None, "user_version", 1).unwrap();
        }
        let err = Store::open_read_only(&path).err().unwrap();
        assert_eq!(err.kind, ErrorKind::Unknown);
        assert!(err.detail.contains("older"), "{}", err.detail);
        // Nothing was migrated behind the reader's back.
        let conn = Connection::open(&path).unwrap();
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, 1);
    }

    #[test]
    fn settings_roundtrip() {
        let (_dir, store) = open_temp();
        assert_eq!(store.load_settings().unwrap(), Settings::default());

        let s = Settings {
            max_active: 7,
            speed: SpeedLimit::Custom { bytes_per_sec: 42 },
            when_done: WhenDone::Shutdown,
            auto_checksum: false,
            keep_awake: false,
        };
        store.save_settings(&s).unwrap();
        assert_eq!(store.load_settings().unwrap(), s);
        let s2 = Settings { max_active: 1, ..s };
        store.save_settings(&s2).unwrap();
        assert_eq!(store.load_settings().unwrap(), s2);

        assert_eq!(store.measured_peak().unwrap(), None);
        store.save_measured_peak(40_000_000, 1_700_000_000).unwrap();
        assert_eq!(
            store.measured_peak().unwrap(),
            Some((40_000_000, 1_700_000_000))
        );
        store.save_measured_peak(5, 6).unwrap();
        assert_eq!(store.measured_peak().unwrap(), Some((5, 6)));
    }

    #[test]
    fn malformed_settings_are_an_error_not_a_reset() {
        let (_dir, store) = open_temp();
        {
            let conn = store.conn.lock().unwrap();
            conn.execute(
                "INSERT INTO settings (key, value) VALUES ('settings', 'not json')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO settings (key, value) VALUES ('measured_peak', 'garbage')",
                [],
            )
            .unwrap();
        }
        assert_eq!(store.load_settings().unwrap_err().kind, ErrorKind::Unknown);
        assert_eq!(store.measured_peak().unwrap_err().kind, ErrorKind::Unknown);
    }

    #[test]
    fn host_cap_roundtrip() {
        let (_dir, store) = open_temp();
        assert_eq!(store.host_cap("example.test").unwrap(), None);
        store.set_host_cap("example.test", 2).unwrap();
        store.set_host_cap("other.test:8080", 5).unwrap();
        assert_eq!(store.host_cap("example.test").unwrap(), Some(2));
        store.set_host_cap("example.test", 1).unwrap();
        assert_eq!(store.host_cap("example.test").unwrap(), Some(1));
        assert_eq!(store.host_cap("other.test:8080").unwrap(), Some(5));
    }

    #[test]
    fn host_cap_is_never_raised() {
        let (_dir, store) = open_temp();
        // Saves arriving in any order keep the lowest cap.
        store.set_host_cap("example.test", 1).unwrap();
        store.set_host_cap("example.test", 3).unwrap();
        assert_eq!(store.host_cap("example.test").unwrap(), Some(1));
        // Concurrent saves from clones (jobs) do too.
        let threads: Vec<_> = (1..=8)
            .rev()
            .map(|cap| {
                let store = store.clone();
                std::thread::spawn(move || store.set_host_cap("busy.test", cap).unwrap())
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        assert_eq!(store.host_cap("busy.test").unwrap(), Some(1));
    }

    #[test]
    fn stale_host_cap_is_ignored() {
        let (_dir, store) = open_temp();
        let day = 24 * 60 * 60;
        store
            .set_host_cap_at("old.test", 1, now_secs() - 8 * day)
            .unwrap();
        assert_eq!(store.host_cap("old.test").unwrap(), None);
        store
            .set_host_cap_at("recent.test", 2, now_secs() - 6 * day)
            .unwrap();
        assert_eq!(store.host_cap("recent.test").unwrap(), Some(2));
        // A new refusal starts it over, at the new cap.
        store.set_host_cap("old.test", 4).unwrap();
        assert_eq!(store.host_cap("old.test").unwrap(), Some(4));
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
