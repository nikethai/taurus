//! A worker: streams one segment's window from one URL into the partial
//! file, checking every response before writing it and retrying
//! transient errors from the durable position.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use reqwest::StatusCode;
use reqwest::header::{CONTENT_RANGE, CONTENT_TYPE, ETAG, LAST_MODIFIED, RANGE, REFERER};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::errors::{CoreError, ErrorKind, backoff_delay_with};
use crate::http::client::{bot_check_error, request_error, status_error};
use crate::http::probe::{parse_content_range, validator_change};
use crate::limit::Limiter;
use crate::page;
use crate::timing::Timing;

use super::part_file::PartFile;
use super::window::{Report, Window, lock};
use super::{error, io_error};

/// A worker syncs and reports progress after this long or this many bytes.
const SYNC_INTERVAL: Duration = Duration::from_secs(1);
const SYNC_BYTES: u64 = 4 * 1024 * 1024;

pub(super) struct WorkerExit {
    /// The URL the worker used.
    pub(super) source: usize,
    /// Durable progress of the segment when the worker stopped.
    pub(super) done: u64,
    pub(super) end: WorkerEnd,
}

pub(super) enum WorkerEnd {
    Complete,
    Cancelled,
    /// The host refused this connection while serving another of the job's:
    /// the segment waits for a free worker. Not a failed attempt.
    ConnectionLimit,
    /// A ranged request got the whole file; the job must restart unranged.
    Fallback,
    /// The worker's URL failed for good: a non-transient error, or out of
    /// attempts.
    SourceFailed(CoreError),
    /// The job cannot go on, whatever URL it uses (e.g. the disk is full).
    Failed(CoreError),
}

/// Why a request failed: the URL (network, server, response) or this
/// machine (the partial file).
enum Fault {
    Source(CoreError),
    Local(CoreError),
}

pub(super) struct Worker {
    pub(super) idx: usize,
    /// Index of `url` in the job's sources.
    pub(super) source: usize,
    pub(super) client: reqwest::Client,
    pub(super) url: String,
    pub(super) referer: Option<String>,
    /// The job's validators, which every 206 must match.
    pub(super) etag: Option<String>,
    pub(super) last_modified: Option<String>,
    pub(super) tapa: PathBuf,
    pub(super) start: u64,
    /// Absolute offsets: bytes written so far, and how many of them are synced.
    pub(super) written: u64,
    pub(super) synced: u64,
    pub(super) last_sync: Instant,
    pub(super) window: Arc<Mutex<Window>>,
    pub(super) resumable: bool,
    pub(super) total: Option<u64>,
    pub(super) stop: CancellationToken,
    pub(super) tx: mpsc::UnboundedSender<Report>,
    pub(super) limiter: Limiter,
    pub(super) timing: Timing,
    /// The job's 206 responses from `url` being read, this worker's included.
    pub(super) open: Arc<AtomicUsize>,
}

impl Worker {
    pub(super) async fn run(mut self) -> WorkerExit {
        let end = match PartFile::open(&self.tapa).await {
            Ok(file) => self.download(file).await,
            Err(e) => WorkerEnd::Failed(io_error(e)),
        };
        WorkerExit {
            source: self.source,
            done: self.synced - self.start,
            end,
        }
    }

    /// Fetches until the window is complete, retrying transient errors from
    /// the durable position with backoff.
    async fn download(&mut self, file: PartFile) -> WorkerEnd {
        // Moved over from another URL: an unranged stream starts over.
        if !self.resumable
            && self.written > 0
            && let Err(e) = self.restart(&file).await
        {
            return WorkerEnd::Failed(e);
        }
        let mut attempts = Attempts::new(self.written);
        loop {
            let fetched = self.fetch(&file).await;
            // Keep whatever was written, even on error: it is the resume point.
            // `written` only counts writes that succeeded, so this never
            // records bytes that did not reach the file.
            let synced = self.sync(&file).await;
            let e = match (fetched, synced) {
                (Ok(end), Ok(())) => return end,
                (Err(Fault::Source(e)), Ok(())) => e,
                // No URL helps against a local error; a failed write (e.g.
                // disk full) is the root cause of a failed sync.
                (Err(Fault::Local(e)), _) | (_, Err(e)) => return WorkerEnd::Failed(e),
            };
            let attempt = attempts.failed_at(self.written);
            if !e.kind.is_transient() || attempt >= self.timing.max_segment_attempts {
                return WorkerEnd::SourceFailed(e);
            }
            tracing::debug!(segment = self.idx, attempt, "retrying: {e}");
            let delay = backoff_delay_with(
                attempt - 1,
                self.timing.backoff_base,
                self.timing.backoff_cap,
            );
            tokio::select! {
                _ = self.stop.cancelled() => return WorkerEnd::Cancelled,
                _ = tokio::time::sleep(delay) => {}
            }
            if !self.resumable
                && let Err(e) = self.restart(&file).await
            {
                return WorkerEnd::Failed(e);
            }
        }
    }

    /// One request, streamed into the file from `written` up to the window end.
    async fn fetch(&mut self, file: &PartFile) -> Result<WorkerEnd, Fault> {
        let end = lock(&self.window).end;
        if self.written >= end {
            return Ok(WorkerEnd::Complete);
        }
        let mut request = self.client.get(&self.url);
        if self.resumable {
            request = request.header(RANGE, format!("bytes={}-{}", self.written, end - 1));
        }
        if let Some(referer) = &self.referer {
            request = request.header(REFERER, referer);
        }
        let mut response = tokio::select! {
            _ = self.stop.cancelled() => return Ok(WorkerEnd::Cancelled),
            sent = request.send() => sent.map_err(|e| Fault::Source(request_error(e)))?,
        };
        if let Some(end) = self.check_response(&response, end).map_err(Fault::Source)? {
            return Ok(end);
        }
        // Counted before it is announced, so the workers it lets start see it.
        let _open = (response.status() == StatusCode::PARTIAL_CONTENT).then(|| {
            let open = OpenResponse::enter(&self.open);
            let _ = self.tx.send(Report::Opened);
            open
        });
        loop {
            let chunk = tokio::select! {
                _ = self.stop.cancelled() => return Ok(WorkerEnd::Cancelled),
                chunk = response.chunk() => chunk.map_err(|e| Fault::Source(request_error(e)))?,
            };
            let Some(chunk) = chunk else { break };
            // Not reading meanwhile is what slows the server down.
            tokio::select! {
                _ = self.stop.cancelled() => return Ok(WorkerEnd::Cancelled),
                () = self.limiter.acquire(chunk.len() as u64) => {}
            }
            let take = self.reserve(chunk.len() as u64);
            if let Err(e) = file
                .write_at(self.written, chunk.slice(..take as usize))
                .await
            {
                // None of this chunk counts: hand the reservation back.
                lock(&self.window).pos = self.written;
                return Err(Fault::Local(io_error(e)));
            }
            self.written += take;
            if self.written >= lock(&self.window).end {
                return Ok(WorkerEnd::Complete);
            }
            if self.written - self.synced >= SYNC_BYTES || self.last_sync.elapsed() >= SYNC_INTERVAL
            {
                self.sync(file).await.map_err(Fault::Local)?;
            }
        }
        // The body ended before the window did. Only an unranged stream of
        // unknown (or reached) size is complete; anything else was cut short.
        let complete = !self.resumable && self.total.is_none_or(|t| self.written >= t);
        if complete {
            Ok(WorkerEnd::Complete)
        } else {
            Err(Fault::Source(error(
                ErrorKind::ConnectionLost,
                format!("server closed the response early at byte {}", self.written),
            )))
        }
    }

    /// Accepts the response, asks for the fallback, or returns the error.
    /// Runs before any byte of the response is written.
    fn check_response(
        &self,
        response: &reqwest::Response,
        end: u64,
    ) -> Result<Option<WorkerEnd>, CoreError> {
        let status = response.status();
        let headers = response.headers();
        // Bodies are not read here, so only the header can tell.
        if page::is_challenge_header(headers) {
            return Err(bot_check_error(status));
        }
        // The probe never admits an HTML job, so a page here is not the file.
        let content_type = headers.get(CONTENT_TYPE).and_then(|v| v.to_str().ok());
        if matches!(status, StatusCode::OK | StatusCode::PARTIAL_CONTENT)
            && page::is_html(content_type)
        {
            return Err(error(
                ErrorKind::CaptivePortal,
                format!("server sent a web page instead of file data (HTTP {status})"),
            ));
        }
        // Another worker's response from this URL is being served, so the
        // host is up: it limits connections. (This worker holds no open
        // response here.) A refusal from a URL serving nothing to the job
        // is an ordinary error, e.g. an expired mirror link.
        if matches!(
            status,
            StatusCode::TOO_MANY_REQUESTS | StatusCode::SERVICE_UNAVAILABLE | StatusCode::FORBIDDEN
        ) && self.open.load(Ordering::SeqCst) > 0
        {
            return Ok(Some(WorkerEnd::ConnectionLimit));
        }
        if !status.is_success() {
            return Err(status_error(status));
        }
        match status {
            StatusCode::OK if !self.resumable => Ok(None),
            StatusCode::PARTIAL_CONTENT if self.resumable => {
                let range = response
                    .headers()
                    .get(CONTENT_RANGE)
                    .and_then(|v| v.to_str().ok())
                    .and_then(parse_content_range);
                let (first, total) = range.map_or((None, None), |r| (r.first, r.total));
                if total.is_some() && total != self.total {
                    // Another size means another file: never stitch two versions.
                    return Err(error(
                        ErrorKind::FileChangedOnServer,
                        format!(
                            "server now reports {total:?} bytes, expected {:?}",
                            self.total
                        ),
                    ));
                }
                // Same size, another version (or another file on a mirror).
                let header = |name| headers.get(name).and_then(|v| v.to_str().ok());
                if let Some(change) = validator_change(
                    (self.etag.as_deref(), self.last_modified.as_deref()),
                    (header(ETAG), header(LAST_MODIFIED)),
                ) {
                    return Err(error(ErrorKind::FileChangedOnServer, change));
                }
                if first == Some(self.written) {
                    Ok(None)
                } else {
                    Err(error(
                        ErrorKind::ConnectionLost,
                        format!("asked for byte {}, server sent {first:?}", self.written),
                    ))
                }
            }
            // A full body is usable only if this segment is the whole file.
            StatusCode::OK if self.written == 0 && self.total.is_some_and(|t| end >= t) => Ok(None),
            StatusCode::OK => Ok(Some(WorkerEnd::Fallback)),
            _ => Err(status_error(status)),
        }
    }

    /// Claims up to `len` bytes of the window; fewer at its end.
    fn reserve(&self, len: u64) -> u64 {
        let mut w = lock(&self.window);
        let take = len.min(w.end.saturating_sub(w.pos));
        w.pos += take;
        take
    }

    /// Makes the written bytes durable, then reports them.
    async fn sync(&mut self, file: &PartFile) -> Result<(), CoreError> {
        if self.written == self.synced {
            return Ok(());
        }
        file.sync_data().await.map_err(io_error)?;
        self.synced = self.written;
        self.last_sync = Instant::now();
        self.report();
        Ok(())
    }

    /// An unranged stream cannot resume: drop what it wrote and start at 0.
    /// The zero is reported before truncating; a job that is not resumable
    /// never resumes from recorded progress, so the order is safe.
    async fn restart(&mut self, file: &PartFile) -> Result<(), CoreError> {
        lock(&self.window).pos = 0;
        self.written = 0;
        self.synced = 0;
        self.report();
        file.set_len(0).await.map_err(io_error)
    }

    fn report(&self) {
        let _ = self.tx.send(Report::Durable {
            idx: self.idx,
            done: self.synced - self.start,
        });
    }
}

/// Counts one 206 response in its source's open responses while held.
struct OpenResponse(Arc<AtomicUsize>);

impl OpenResponse {
    fn enter(open: &Arc<AtomicUsize>) -> OpenResponse {
        open.fetch_add(1, Ordering::SeqCst);
        OpenResponse(open.clone())
    }
}

impl Drop for OpenResponse {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Consecutive failed attempts of one worker. Only progress past the furthest
/// byte reached so far resets the count: an unranged stream restarts from 0,
/// so re-downloading up to the same point again is not progress.
struct Attempts {
    failed: u32,
    high_water: u64,
}

impl Attempts {
    fn new(written: u64) -> Attempts {
        Attempts {
            failed: 0,
            high_water: written,
        }
    }

    /// Records a failed attempt that ended with `written` bytes written;
    /// returns the number of consecutive failures (1-based).
    fn failed_at(&mut self, written: u64) -> u32 {
        if written > self.high_water {
            self.high_water = written;
            self.failed = 0;
        }
        self.failed += 1;
        self.failed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attempts_reset_only_past_the_high_water_mark() {
        // Ranged: a failure after new progress counts as a first attempt.
        let mut attempts = Attempts::new(0);
        assert_eq!(attempts.failed_at(1000), 1);
        assert_eq!(attempts.failed_at(2000), 1);
        assert_eq!(attempts.failed_at(2000), 2);

        // Unranged restarts re-download from 0: getting back to the same
        // byte is not progress, so the attempts run out.
        let mut attempts = Attempts::new(0);
        let max = Timing::default().max_segment_attempts;
        let counts: Vec<u32> = (0..max).map(|_| attempts.failed_at(5000)).collect();
        assert_eq!(counts, (1..=max).collect::<Vec<_>>());
        assert_eq!(attempts.failed_at(6000), 1);
    }
}
