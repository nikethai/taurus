//! The database schema, its migration chain and opening a database safely.

use std::path::Path;
use std::sync::{Arc, Mutex};

use rusqlite::{Connection, OpenFlags};

use crate::errors::{CoreError, ErrorKind};

use super::{Store, db_err, io_err, now_secs};

const SCHEMA_VERSION: i64 = 7;

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

/// Applied on top of v5: media jobs (kind 'media') and what they download,
/// as JSON. The state 'recording' needs no change: states are text.
const MIGRATE_V5_TO_V6: &str = "
ALTER TABLE jobs ADD COLUMN media_options TEXT;
";

/// Applied on top of v6: one step for all of M6a's columns (v7 is not
/// released until M6a is; each of its additions appends an `ALTER` here).
/// `auth_user`: the user name of a protected download (its password is in
/// the secret store, never here).
/// `convert_source`: what a convert job (kind 'convert') converts, as JSON
/// (source file, preset, and the partial output of its run).
const MIGRATE_V6_TO_V7: &str = "
ALTER TABLE jobs ADD COLUMN auth_user TEXT;
ALTER TABLE jobs ADD COLUMN convert_source TEXT;
";

fn is_corruption(e: &rusqlite::Error) -> bool {
    matches!(
        e.sqlite_error_code(),
        Some(rusqlite::ErrorCode::NotADatabase | rusqlite::ErrorCode::DatabaseCorrupt)
    )
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
    let chain = [
        SCHEMA_V1,
        MIGRATE_V1_TO_V2,
        MIGRATE_V2_TO_V3,
        MIGRATE_V3_TO_V4,
        MIGRATE_V4_TO_V5,
        MIGRATE_V5_TO_V6,
        MIGRATE_V6_TO_V7,
    ];
    let steps = match usize::try_from(version) {
        Ok(from) if from < chain.len() => chain[from..].concat(),
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::convert::ConvertSpec;
    use crate::media::ConvertPreset;
    use crate::store::test_support::{new_job, open_temp};
    use crate::store::{JobKind, JobState, NewJob};

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
        assert_eq!(version, SCHEMA_VERSION);
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
            assert_eq!(version, SCHEMA_VERSION);
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
        assert_eq!(version, SCHEMA_VERSION);
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
        assert_eq!(version, SCHEMA_VERSION);
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
        assert_eq!(version, SCHEMA_VERSION);
        drop(store);
        assert_eq!(Store::open(&path).unwrap().list_jobs().unwrap().len(), 1);
    }

    #[test]
    fn v5_database_migrates_to_v6() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("taurus.db");
        {
            let conn = Connection::open(&path).unwrap();
            for step in [
                SCHEMA_V1,
                MIGRATE_V1_TO_V2,
                MIGRATE_V2_TO_V3,
                MIGRATE_V3_TO_V4,
                MIGRATE_V4_TO_V5,
            ] {
                conn.execute_batch(step).unwrap();
            }
            conn.pragma_update(None, "user_version", 5).unwrap();
            conn.execute(
                "INSERT INTO jobs (url, source_url, dest_dir, file_name, state, connections, \
                 created_at, updated_at, kind, info_hash, uploaded) \
                 VALUES ('magnet:?x', 'magnet:?x', '/d', 't', 'seeding', 1, 1, 1, 'torrent', \
                 'abcd', 42)",
                [],
            )
            .unwrap();
        }

        let store = Store::open(&path).unwrap();
        let jobs = store.list_jobs().unwrap();
        assert_eq!(jobs.len(), 1);
        let mut job = jobs[0].clone();
        // What v5 recorded survives; no job has media options yet.
        assert_eq!(job.kind, JobKind::Torrent);
        assert_eq!(job.state, JobState::Seeding);
        assert_eq!(job.info_hash.as_deref(), Some("abcd"));
        assert_eq!(job.uploaded, 42);
        assert_eq!(job.media, None);
        let version: i64 = store
            .lock()
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        // The new kind, state and column are usable.
        let options = crate::media::MediaOptions {
            quality: "audio-best".into(),
            selector: "ba/b".into(),
            audio_only: true,
            convert: Some(crate::media::ConvertPreset::Mp3),
            playlist: false,
            live_from_start: true,
            partial_files: vec!["/d/a [x].mp4.part".into()],
            ..Default::default()
        };
        let id = store
            .insert_job(&crate::store::NewJob {
                kind: JobKind::Media,
                ..new_job("m", "/d")
            })
            .unwrap();
        let mut media = store.get_job(id).unwrap().unwrap();
        media.media = Some(options.clone());
        media.state = JobState::Recording;
        store.update_job(&media).unwrap();
        let stored = store.get_job(id).unwrap().unwrap();
        assert_eq!(stored, media);
        assert_eq!(stored.kind, JobKind::Media);
        job.media = None;
        store.update_job(&job).unwrap();
        drop(store);
        let store = Store::open(&path).unwrap();
        assert_eq!(
            store.get_job(id).unwrap().unwrap().media,
            Some(options.clone())
        );
        assert_eq!(store.list_jobs().unwrap().len(), 2);
        // Only the media options change; options stored before the partial
        // files were recorded read without them (an older `video_ids` is
        // ignored).
        let more = crate::media::MediaOptions {
            partial_files: vec!["/d/a [x].mp4.part".into(), "/d/a [x].mp4".into()],
            ..options
        };
        store.set_media(id, &more).unwrap();
        let stored = store.get_job(id).unwrap().unwrap();
        assert_eq!(stored.media, Some(more));
        assert_eq!(stored.state, JobState::Recording);
        let old: crate::media::MediaOptions = serde_json::from_str(
            r#"{"quality":"best","selector":"b","audio_only":false,"convert":null,
                "playlist":false,"live_from_start":false,"video_ids":["abc"]}"#,
        )
        .unwrap();
        assert!(old.partial_files.is_empty());
    }

    #[test]
    fn v6_database_migrates_to_v7() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("taurus.db");
        {
            let conn = Connection::open(&path).unwrap();
            for step in [
                SCHEMA_V1,
                MIGRATE_V1_TO_V2,
                MIGRATE_V2_TO_V3,
                MIGRATE_V3_TO_V4,
                MIGRATE_V4_TO_V5,
                MIGRATE_V5_TO_V6,
            ] {
                conn.execute_batch(step).unwrap();
            }
            conn.pragma_update(None, "user_version", 6).unwrap();
            conn.execute(
                "INSERT INTO jobs (url, source_url, dest_dir, file_name, state, connections, \
                 created_at, updated_at, kind, media_options) \
                 VALUES ('u', 'u', '/d', 'x', 'paused', 4, 1, 1, 'media', \
                 '{\"quality\":\"best\",\"selector\":\"b\",\"audio_only\":false,\
                 \"convert\":null,\"playlist\":false,\"live_from_start\":false}')",
                [],
            )
            .unwrap();
        }

        let store = Store::open(&path).unwrap();
        let jobs = store.list_jobs().unwrap();
        assert_eq!(jobs.len(), 1);
        let mut job = jobs[0].clone();
        // What v6 recorded survives; no job has a user name yet.
        assert_eq!(job.kind, JobKind::Media);
        assert_eq!(job.state, JobState::Paused);
        assert_eq!(job.media.as_ref().map(|m| m.quality.as_str()), Some("best"));
        assert_eq!(job.auth_user, None);
        assert_eq!(job.convert, None);
        let version: i64 = store
            .lock()
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, 7);
        // The user name is set on its own; a whole-record update from a
        // copy without it leaves it alone.
        store.set_auth_user(job.id, Some("ann")).unwrap();
        job.state = JobState::Failed;
        store.update_job(&job).unwrap();
        let stored = store.get_job(job.id).unwrap().unwrap();
        assert_eq!(stored.auth_user.as_deref(), Some("ann"));
        assert_eq!(stored.state, JobState::Failed);
        drop(store);
        let store = Store::open(&path).unwrap();
        assert_eq!(
            store.get_job(job.id).unwrap().unwrap().auth_user.as_deref(),
            Some("ann")
        );
        store.set_auth_user(job.id, None).unwrap();
        assert_eq!(store.get_job(job.id).unwrap().unwrap().auth_user, None);

        // What a convert job converts is set on its own, too.
        let spec = ConvertSpec {
            source: "/d/song.webm".into(),
            preset: ConvertPreset::Mp3,
            partial: Some("/d/.part.mp3".into()),
        };
        store.set_convert(job.id, &spec).unwrap();
        job.state = JobState::Paused;
        store.update_job(&job).unwrap();
        let stored = store.get_job(job.id).unwrap().unwrap();
        assert_eq!(stored.convert.as_ref(), Some(&spec));
        assert_eq!(stored.state, JobState::Paused);
        drop(store);
        let store = Store::open(&path).unwrap();
        let stored = store.get_job(job.id).unwrap().unwrap();
        assert_eq!(stored.convert, Some(spec));
        // A new job of the new kind roundtrips.
        let id = store
            .insert_job(&NewJob {
                kind: JobKind::Convert,
                ..new_job("song (mp3).mp3", "/d")
            })
            .unwrap();
        assert_eq!(store.get_job(id).unwrap().unwrap().kind, JobKind::Convert);
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
