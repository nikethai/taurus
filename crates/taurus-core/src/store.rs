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

const SCHEMA_VERSION: i64 = 5;

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

/// Applied on top of v2: the user's own link, before fixups and forms.
const MIGRATE_V2_TO_V3: &str = "
ALTER TABLE jobs ADD COLUMN source_url TEXT;
UPDATE jobs SET source_url = url;
";

/// Applied on top of v3: jobs to check against the server before they run
/// again, because a crash, kill or shutdown interrupted them.
const MIGRATE_V3_TO_V4: &str = "
ALTER TABLE jobs ADD COLUMN revalidate INTEGER NOT NULL DEFAULT 0;
";

/// Applied on top of v4: torrent jobs. One job per info-hash.
const MIGRATE_V4_TO_V5: &str = "
ALTER TABLE jobs ADD COLUMN kind TEXT NOT NULL DEFAULT 'http';
ALTER TABLE jobs ADD COLUMN info_hash TEXT;
ALTER TABLE jobs ADD COLUMN torrent_meta BLOB;
ALTER TABLE jobs ADD COLUMN only_files TEXT;
ALTER TABLE jobs ADD COLUMN uploaded INTEGER NOT NULL DEFAULT 0;
ALTER TABLE jobs ADD COLUMN seed_started_at INTEGER;
CREATE UNIQUE INDEX jobs_info_hash ON jobs(info_hash) WHERE info_hash IS NOT NULL;
";

const SETTINGS_KEY: &str = "settings";
/// Where `load_settings_lenient` keeps saved settings it could not read.
const UNREADABLE_SETTINGS_KEY: &str = "settings.unreadable";
const MEASURED_PEAK_KEY: &str = "measured_peak";

const JOB_COLUMNS: &str = "id, url, final_url, dest_dir, file_name, state, total, resumable, \
                           etag, last_modified, connections, error_kind, error_detail, \
                           mirrors, checksum, referer, queue_pos, source_url, kind, info_hash, \
                           torrent_meta, only_files, uploaded, seed_started_at";

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

fn only_files_json(only_files: Option<&[usize]>) -> Result<Option<String>, CoreError> {
    only_files
        .map(|files| {
            serde_json::to_string(files).map_err(|e| CoreError {
                kind: ErrorKind::Unknown,
                detail: e.to_string(),
            })
        })
        .transpose()
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
    let url: String = row.get(1)?;
    let source_url: Option<String> = row.get(17)?;
    let kind: String = row.get(18)?;
    let kind = JobKind::parse(&kind).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            18,
            rusqlite::types::Type::Text,
            format!("unknown job kind {kind:?}").into(),
        )
    })?;
    let only_files: Option<String> = row.get(21)?;
    let only_files = only_files
        .map(|json| {
            serde_json::from_str(&json).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    21,
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            })
        })
        .transpose()?;
    Ok(JobRecord {
        id: JobId(row.get(0)?),
        source_url: source_url.unwrap_or_else(|| url.clone()),
        url,
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
        kind,
        info_hash: row.get(19)?,
        torrent_meta: row.get(20)?,
        only_files,
        uploaded: col_u64(row, 22)?,
        seed_started_at: row.get(23)?,
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
    let steps = match version {
        0 => [
            SCHEMA_V1,
            MIGRATE_V1_TO_V2,
            MIGRATE_V2_TO_V3,
            MIGRATE_V3_TO_V4,
            MIGRATE_V4_TO_V5,
        ]
        .concat(),
        1 => [
            MIGRATE_V1_TO_V2,
            MIGRATE_V2_TO_V3,
            MIGRATE_V3_TO_V4,
            MIGRATE_V4_TO_V5,
        ]
        .concat(),
        2 => [MIGRATE_V2_TO_V3, MIGRATE_V3_TO_V4, MIGRATE_V4_TO_V5].concat(),
        3 => [MIGRATE_V3_TO_V4, MIGRATE_V4_TO_V5].concat(),
        4 => MIGRATE_V4_TO_V5.to_owned(),
        _ => return Ok(conn),
    };
    conn.execute_batch(&format!(
        "BEGIN; {steps} PRAGMA user_version = {SCHEMA_VERSION}; COMMIT;"
    ))?;
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

    /// Sets only the bytes the torrent has uploaded in all, so no other
    /// field can be overwritten from a stale copy. Updating a job that no
    /// longer exists is a no-op.
    pub fn commit_torrent_progress(&self, id: JobId, uploaded: u64) -> Result<(), CoreError> {
        let uploaded = to_i64(uploaded, "uploaded bytes")?;
        self.lock()
            .execute(
                "UPDATE jobs SET uploaded = ?2, updated_at = ?3 WHERE id = ?1",
                params![id.0, uploaded, now_secs()],
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
    let reads = |fields: &serde_json::Map<String, serde_json::Value>| {
        serde_json::from_value::<Settings>(serde_json::Value::Object(fields.clone())).is_ok()
    };
    for (key, value) in saved {
        if !fields.contains_key(&key) {
            continue;
        }
        let mut tried = fields.clone();
        tried.insert(key.clone(), value.clone());
        if reads(&tried) {
            fields.insert(key, value);
        } else if let (
            Some(serde_json::Value::Object(mut section)),
            serde_json::Value::Object(saved),
        ) = (fields.get(&key).cloned(), value)
        {
            // A section (`torrent`) keeps its readable fields too.
            for (name, value) in saved {
                if !section.contains_key(&name) {
                    continue;
                }
                let before = section.insert(name.clone(), value);
                tried.insert(key.clone(), serde_json::Value::Object(section.clone()));
                if !reads(&tried) {
                    section.extend(before.map(|v| (name, v)));
                }
            }
            fields.insert(key, serde_json::Value::Object(section));
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
    use crate::settings::{SpeedLimit, TorrentSettings, WhenDone};

    fn open_temp() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("taurus.db")).unwrap();
        (dir, store)
    }

    fn new_job(name: &str, dest: &str) -> NewJob {
        NewJob {
            url: format!("http://example.test/{name}"),
            source_url: format!("http://example.test/share/{name}"),
            dest_dir: PathBuf::from(dest),
            file_name: name.to_string(),
            connections: 8,
            mirrors: Vec::new(),
            checksum: None,
            referer: None,
            queue_pos: 0,
            kind: JobKind::Http,
            info_hash: None,
            torrent_meta: None,
            only_files: None,
        }
    }

    fn torrent_job(name: &str, info_hash: &str) -> NewJob {
        NewJob {
            kind: JobKind::Torrent,
            info_hash: Some(info_hash.into()),
            torrent_meta: Some(vec![0, 1, 2, 255]),
            only_files: Some(vec![0, 2]),
            ..new_job(name, "/d")
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
        assert_eq!(version, 5);
    }

    #[test]
    fn v1_database_migrates_to_v5() {
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
            assert_eq!(job.source_url, job.url);
            assert_eq!(job.queue_pos, job.id.0);
            assert_eq!((job.checksum.clone(), job.referer.clone()), (None, None));
            assert_eq!(job.state, JobState::Paused);
            assert!(!store.needs_revalidation(job.id).unwrap());
        }
        {
            let conn = store.conn.lock().unwrap();
            let version: i64 = conn
                .query_row("PRAGMA user_version", [], |r| r.get(0))
                .unwrap();
            assert_eq!(version, 5);
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
    fn v2_database_migrates_to_v5() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("taurus.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(SCHEMA_V1).unwrap();
            conn.execute_batch(MIGRATE_V1_TO_V2).unwrap();
            conn.pragma_update(None, "user_version", 2).unwrap();
            conn.execute(
                "INSERT INTO jobs (url, dest_dir, file_name, state, connections, created_at, \
                 updated_at, mirrors, referer, queue_pos) \
                 VALUES ('http://a.test/x?confirm=t', '/d', 'x', 'paused', 4, 1, 1, \
                 '[\"http://m.test/x\"]', 'http://page.test/', 7)",
                [],
            )
            .unwrap();
        }

        let store = Store::open(&path).unwrap();
        let jobs = store.list_jobs().unwrap();
        assert_eq!(jobs.len(), 1);
        let job = &jobs[0];
        // Unknown before v3: the stored link is the best guess.
        assert_eq!(job.source_url, "http://a.test/x?confirm=t");
        assert_eq!(job.url, job.source_url);
        assert_eq!(job.mirrors, ["http://m.test/x"]);
        assert_eq!(job.referer.as_deref(), Some("http://page.test/"));
        assert_eq!(job.queue_pos, 7);
        let version: i64 = store
            .lock()
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, 5);
        drop(store);
        assert_eq!(Store::open(&path).unwrap().list_jobs().unwrap().len(), 1);
    }

    #[test]
    fn v3_database_migrates_to_v5() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("taurus.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(SCHEMA_V1).unwrap();
            conn.execute_batch(MIGRATE_V1_TO_V2).unwrap();
            conn.execute_batch(MIGRATE_V2_TO_V3).unwrap();
            conn.pragma_update(None, "user_version", 3).unwrap();
            conn.execute(
                "INSERT INTO jobs (url, source_url, dest_dir, file_name, state, connections, \
                 created_at, updated_at) VALUES ('u', 'u', '/d', 'x', 'queued', 4, 1, 1)",
                [],
            )
            .unwrap();
        }

        let store = Store::open(&path).unwrap();
        let jobs = store.list_jobs().unwrap();
        assert_eq!(jobs.len(), 1);
        assert!(!store.needs_revalidation(jobs[0].id).unwrap());
        let version: i64 = store
            .lock()
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, 5);
    }

    #[test]
    fn v4_database_migrates_to_v5() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("taurus.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(SCHEMA_V1).unwrap();
            conn.execute_batch(MIGRATE_V1_TO_V2).unwrap();
            conn.execute_batch(MIGRATE_V2_TO_V3).unwrap();
            conn.execute_batch(MIGRATE_V3_TO_V4).unwrap();
            conn.pragma_update(None, "user_version", 4).unwrap();
            conn.execute(
                "INSERT INTO jobs (url, source_url, dest_dir, file_name, state, connections, \
                 created_at, updated_at, revalidate) \
                 VALUES ('u', 'u', '/d', 'x', 'queued', 4, 1, 1, 1)",
                [],
            )
            .unwrap();
        }

        let store = Store::open(&path).unwrap();
        let jobs = store.list_jobs().unwrap();
        assert_eq!(jobs.len(), 1);
        let job = &jobs[0];
        assert_eq!(job.kind, JobKind::Http);
        assert_eq!(job.uploaded, 0);
        assert_eq!(
            (&job.info_hash, &job.torrent_meta, &job.only_files),
            (&None, &None, &None)
        );
        assert_eq!(job.seed_started_at, None);
        // What v4 recorded survives.
        assert!(store.needs_revalidation(job.id).unwrap());
        let version: i64 = store
            .lock()
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, 5);
        drop(store);
        assert_eq!(Store::open(&path).unwrap().list_jobs().unwrap().len(), 1);
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
        store.commit_torrent_progress(id, 5_000_000_000).unwrap();
        let stored = store.get_job(id).unwrap().unwrap();
        assert_eq!(stored.uploaded, 5_000_000_000);
        assert_eq!(
            JobRecord {
                uploaded: 0,
                ..stored.clone()
            },
            job
        );
        assert!(store.commit_torrent_progress(id, u64::MAX).is_err());
        // A stale whole-record update leaves them alone.
        store.update_job(&job).unwrap();
        assert_eq!(store.get_job(id).unwrap().unwrap().uploaded, 5_000_000_000);
        // A job that is gone is not an error.
        store.delete_job(id).unwrap();
        store.commit_torrent_progress(id, 1).unwrap();
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
    fn lenient_settings_keep_readable_torrent_fields() {
        let (_dir, store) = open_temp();
        let saved = r#"{"max_active":4,"torrent":{"stop_ratio":2.5,"max_active":"many","dht":false,"bogus":1}}"#;
        store.put_setting(SETTINGS_KEY, saved).unwrap();
        let s = store.load_settings_lenient().unwrap();
        assert_eq!(s.max_active, 4);
        assert_eq!(
            s.torrent,
            TorrentSettings {
                stop_ratio: Some(2.5),
                dht: false,
                ..TorrentSettings::default()
            }
        );
        // A torrent section that is not an object falls back to the defaults.
        store
            .put_setting(SETTINGS_KEY, r#"{"max_active":4,"torrent":7}"#)
            .unwrap();
        let s = store.load_settings_lenient().unwrap();
        assert_eq!((s.max_active, s.torrent), (4, TorrentSettings::default()));
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
    fn read_only_open_refuses_an_older_database() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("taurus.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(SCHEMA_V1).unwrap();
            conn.execute_batch(MIGRATE_V1_TO_V2).unwrap();
            conn.pragma_update(None, "user_version", 2).unwrap();
        }
        let err = Store::open_read_only(&path).err().unwrap();
        assert!(err.detail.contains("older"), "{}", err.detail);
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
            torrent: TorrentSettings {
                stop_ratio: None,
                max_active: 2,
                listen_port: Some(51413),
                ..TorrentSettings::default()
            },
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
