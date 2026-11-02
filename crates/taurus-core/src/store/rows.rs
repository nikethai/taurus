//! Converting between job/segment values and database columns.

use std::path::PathBuf;

use rusqlite::Row;

use crate::errors::{CoreError, ErrorKind};
use crate::plan::Segment;

use super::{JobId, JobKind, JobRecord, JobState, MediaOptions};

pub(super) const JOB_COLUMNS: &str = "id, url, final_url, dest_dir, file_name, state, total, resumable, \
                           etag, last_modified, connections, error_kind, error_detail, \
                           mirrors, checksum, referer, queue_pos, source_url, kind, info_hash, \
                           torrent_meta, only_files, uploaded, seed_started_at, media_options, \
                           auth_user";

pub(super) fn only_files_json(only_files: Option<&[usize]>) -> Result<Option<String>, CoreError> {
    only_files
        .map(|files| {
            serde_json::to_string(files).map_err(|e| CoreError {
                kind: ErrorKind::Unknown,
                detail: e.to_string(),
            })
        })
        .transpose()
}

pub(super) fn media_json(media: Option<&MediaOptions>) -> Result<Option<String>, CoreError> {
    media
        .map(|media| {
            serde_json::to_string(media).map_err(|e| CoreError {
                kind: ErrorKind::Unknown,
                detail: e.to_string(),
            })
        })
        .transpose()
}

pub(super) fn to_i64(v: u64, what: &str) -> Result<i64, CoreError> {
    i64::try_from(v).map_err(|_| CoreError {
        kind: ErrorKind::Unknown,
        detail: format!("{what} {v} does not fit the database"),
    })
}

pub(super) fn opt_to_i64(v: Option<u64>, what: &str) -> Result<Option<i64>, CoreError> {
    v.map(|v| to_i64(v, what)).transpose()
}

/// Reads an INTEGER column as `u64`; a negative value means a damaged row.
pub(super) fn col_u64(row: &Row<'_>, idx: usize) -> rusqlite::Result<u64> {
    let v: i64 = row.get(idx)?;
    u64::try_from(v).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(idx, rusqlite::types::Type::Integer, Box::new(e))
    })
}

pub(super) fn job_from_row(row: &Row<'_>) -> rusqlite::Result<JobRecord> {
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
    let media: Option<String> = row.get(24)?;
    let media = media
        .map(|json| {
            serde_json::from_str(&json).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    24,
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
        media,
        auth_user: row.get(25)?,
    })
}

pub(super) fn mirrors_json(mirrors: &[String]) -> Result<String, CoreError> {
    serde_json::to_string(mirrors).map_err(|e| CoreError {
        kind: ErrorKind::Unknown,
        detail: e.to_string(),
    })
}

/// Encodes a segment as `(start, end, done)` column values; an end of
/// `u64::MAX` (open-ended) becomes NULL.
pub(super) fn encode(seg: &Segment) -> Result<(i64, Option<i64>, i64), CoreError> {
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
