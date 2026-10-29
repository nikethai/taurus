//! Finishing a job whose bytes are all downloaded: the length check, the
//! checksum (the job's own or a `SHA256SUMS` file's), the move to a free
//! final name and the downloaded-from-the-internet mark.

use std::io;
use std::path::Path;
use std::time::Duration;

use reqwest::header::REFERER;
use tokio::fs::OpenOptions;

use crate::checksum::{self, Sha256Sum};
use crate::errors::{CoreError, ErrorKind};
use crate::filename::{self, unique_path};
use crate::store::{JobRecord, JobState};
use crate::system;

use super::{JobContext, JobOutcome, error, io_error};

const RENAME_ATTEMPTS: u32 = 5;
const RENAME_RETRY_DELAY: Duration = Duration::from_millis(200);
/// A `SHA256SUMS` file larger than this is not read.
const SUMS_CAP: u64 = 1024 * 1024;

/// Verifies the length and the checksum, then moves the partial to a free
/// final name and marks it as downloaded from the internet.
pub(super) async fn finish(
    ctx: &JobContext,
    job: &mut JobRecord,
    tapa: &Path,
) -> Result<JobOutcome, CoreError> {
    let file = OpenOptions::new()
        .write(true)
        .open(tapa)
        .await
        .map_err(io_error)?;
    let len = file.metadata().await.map_err(io_error)?.len();
    if let Some(total) = job.total.filter(|&total| total != len) {
        return Err(error(
            ErrorKind::Unknown,
            format!("downloaded file is {len} bytes, expected {total}"),
        ));
    }
    file.sync_all().await.map_err(io_error)?;
    drop(file);

    let looks_up = job.checksum.is_none() && ctx.auto_checksum;
    let expected = expected_sum(ctx, job).await?;
    if looks_up && ctx.cancel.is_cancelled() {
        // Stopped while fetching the sums: verified on the next run.
        return pause_complete(ctx, job);
    }
    if let Some(expected) = expected {
        // A multi-GB file takes minutes to hash; a pause must not wait for
        // it. The blocking read runs on and its result is dropped.
        let got = tokio::select! {
            biased;
            _ = ctx.cancel.cancelled() => return pause_complete(ctx, job),
            got = checksum::hash_file(tapa) => got.map_err(io_error)?,
        };
        if got != expected {
            // Bad bytes are not kept; with no segments the next run starts over.
            if let Err(e) = tokio::fs::remove_file(tapa).await
                && e.kind() != io::ErrorKind::NotFound
            {
                tracing::warn!(job = job.id.0, "could not delete the partial file: {e}");
            }
            ctx.store.replace_segments(job.id, &[])?;
            return Err(error(
                ErrorKind::ChecksumMismatch,
                format!("expected {}, got {}", expected.0, got.0),
            ));
        }
    }

    let mut path = job.dest_dir.join(&job.file_name);
    if path.symlink_metadata().is_ok() {
        // The job's own name is in `reserved`; it is taken on disk anyway.
        let mut reserved = ctx.store.reserved_names(&job.dest_dir)?;
        reserved.remove(&job.file_name);
        path = unique_path(&job.dest_dir, &job.file_name, &reserved);
        if let Some(name) = path.file_name() {
            job.file_name = name.to_string_lossy().into_owned();
        }
        ctx.store.update_job(job)?;
    }
    rename_with_retry(tapa, &path).await?;
    // The link the user gave, as a browser marks the page's link.
    system::mark_downloaded(&path, &job.source_url, job.referer.as_deref());
    job.state = JobState::Completed;
    ctx.store.update_job(job)?;
    Ok(JobOutcome::Completed(path))
}

/// Pauses a job whose bytes are all downloaded but not yet verified; the
/// next run finds its segments complete and verifies it again.
fn pause_complete(ctx: &JobContext, job: &mut JobRecord) -> Result<JobOutcome, CoreError> {
    job.state = JobState::Paused;
    ctx.store.update_job(job)?;
    Ok(JobOutcome::Paused)
}

/// The sum the file must have: the job's own, else (with `auto_checksum`)
/// the one published in a `SHA256SUMS` file next to it, if any.
async fn expected_sum(ctx: &JobContext, job: &JobRecord) -> Result<Option<Sha256Sum>, CoreError> {
    if let Some(given) = &job.checksum {
        return checksum::parse_sum(given)
            .map(Some)
            .ok_or_else(|| error(ErrorKind::Unknown, "invalid checksum"));
    }
    if !ctx.auto_checksum {
        return Ok(None);
    }
    let Ok(url) = url::Url::parse(job.final_url.as_deref().unwrap_or(&job.url)) else {
        return Ok(None);
    };
    let (Some(sums), Some(name)) = (checksum::sums_url(&url), filename::from_url(&url)) else {
        return Ok(None);
    };
    let Some(text) = fetch_sums(ctx, job, sums).await else {
        return Ok(None);
    };
    Ok(checksum::find_in_sums_file(&text, &name))
}

/// The text of a `SHA256SUMS` file, fetched like the job's own requests;
/// `None` on any error, a non-2xx answer, a body over 1 MiB, or cancel.
async fn fetch_sums(ctx: &JobContext, job: &JobRecord, url: url::Url) -> Option<String> {
    let mut request = ctx.client.get(url);
    if let Some(referer) = &job.referer {
        request = request.header(REFERER, referer);
    }
    let mut response = tokio::select! {
        _ = ctx.cancel.cancelled() => return None,
        sent = request.send() => sent.ok()?,
    };
    if !response.status().is_success()
        || response.content_length().is_some_and(|len| len > SUMS_CAP)
    {
        return None;
    }
    let mut body = Vec::new();
    loop {
        let chunk = tokio::select! {
            _ = ctx.cancel.cancelled() => return None,
            chunk = response.chunk() => chunk.ok()?,
        };
        let Some(chunk) = chunk else { break };
        if body.len() + chunk.len() > SUMS_CAP as usize {
            tracing::info!(job = job.id.0, "ignoring a SHA256SUMS file over 1 MiB");
            return None;
        }
        body.extend_from_slice(&chunk);
    }
    Some(String::from_utf8_lossy(&body).into_owned())
}

/// Windows can briefly lock a just-closed file (antivirus, indexer).
async fn rename_with_retry(from: &Path, to: &Path) -> Result<(), CoreError> {
    let mut attempt = 1;
    loop {
        match tokio::fs::rename(from, to).await {
            Ok(()) => return Ok(()),
            Err(e)
                if e.kind() == std::io::ErrorKind::PermissionDenied
                    && attempt < RENAME_ATTEMPTS =>
            {
                attempt += 1;
                tokio::time::sleep(RENAME_RETRY_DELAY).await;
            }
            Err(e) => return Err(io_error(e)),
        }
    }
}
