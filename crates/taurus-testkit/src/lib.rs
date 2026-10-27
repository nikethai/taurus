//! Test support for Taurus: a local HTTP server with scriptable behavior.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::header::{
    ACCEPT_RANGES, CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, ETAG,
    HeaderMap, HeaderValue, LAST_MODIFIED, RANGE,
};
use axum::http::{Response, StatusCode};
use axum::routing::get;
use bytes::Bytes;
use sha2::{Digest, Sha256};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio::time::Instant;

const CHUNK: usize = 16 * 1024;

/// Scripted behavior of one served path.
#[derive(Clone, Debug)]
pub struct FileSpec {
    pub size: usize,
    pub seed: u64,
    /// Honor Range headers.
    pub ranges: bool,
    /// Honor only `bytes=0-0`; answer 200 with the full body otherwise.
    pub ignore_range_after_probe: bool,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub content_disposition: Option<String>,
    /// Defaults to `application/octet-stream`.
    pub content_type: Option<String>,
    /// Aggregate cap shared by all concurrent responses for the path,
    /// paced in 16 KiB chunks.
    pub bytes_per_sec: Option<u64>,
    /// The first response whose byte window contains this absolute offset
    /// aborts the connection after sending the bytes before it.
    pub drop_once_after: Option<usize>,
    /// Request number k (1-based) answers `fail_first[k - 1]` while k <= len.
    pub fail_first: Vec<u16>,
}

impl FileSpec {
    pub fn new(size: usize) -> Self {
        Self {
            size,
            seed: 1,
            ranges: true,
            ignore_range_after_probe: false,
            etag: None,
            last_modified: None,
            content_disposition: None,
            content_type: None,
            bytes_per_sec: None,
            drop_once_after: None,
            fail_first: Vec::new(),
        }
    }
}

/// Deterministic pseudo-random bytes (xorshift64) derived from `spec.seed`.
pub fn content(spec: &FileSpec) -> Vec<u8> {
    const MIX: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut state = spec.seed ^ MIX;
    if state == 0 {
        state = MIX;
    }
    let mut out = Vec::with_capacity(spec.size + 8);
    while out.len() < spec.size {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        out.extend_from_slice(&state.to_le_bytes());
    }
    out.truncate(spec.size);
    out
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

struct Entry {
    spec: FileSpec,
    data: Bytes,
    requests: usize,
    served: Arc<AtomicU64>,
    drop_spent: bool,
    /// Earliest release time of the next chunk; shared by every response.
    pacer: Arc<Mutex<Instant>>,
}

type Shared = Arc<Mutex<HashMap<String, Entry>>>;

fn lock(shared: &Shared) -> MutexGuard<'_, HashMap<String, Entry>> {
    shared.lock().unwrap_or_else(|e| e.into_inner())
}

fn normalize(path: &str) -> String {
    if path.starts_with('/') {
        path.to_owned()
    } else {
        format!("/{path}")
    }
}

fn check_headers(spec: &FileSpec) {
    for value in [
        &spec.etag,
        &spec.last_modified,
        &spec.content_disposition,
        &spec.content_type,
    ]
    .into_iter()
    .flatten()
    {
        HeaderValue::from_str(value).expect("FileSpec header value is not a valid HTTP header");
    }
}

/// A local HTTP server serving scripted files from `127.0.0.1`.
pub struct TestServer {
    base: String,
    shared: Shared,
    task: JoinHandle<()>,
}

impl TestServer {
    pub async fn start() -> TestServer {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test server");
        let addr = listener.local_addr().expect("test server address");
        let shared: Shared = Arc::default();
        let app = Router::new()
            .route("/{*path}", get(handle))
            .with_state(shared.clone());
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        TestServer {
            base: format!("http://{addr}"),
            shared,
            task,
        }
    }

    /// Registers `spec` at `path` and returns the full URL.
    pub fn serve(&self, path: &str, spec: FileSpec) -> String {
        check_headers(&spec);
        let path = normalize(path);
        let entry = Entry {
            data: Bytes::from(content(&spec)),
            spec,
            requests: 0,
            served: Arc::new(AtomicU64::new(0)),
            drop_spent: false,
            pacer: Arc::new(Mutex::new(Instant::now())),
        };
        lock(&self.shared).insert(path.clone(), entry);
        format!("{}{path}", self.base)
    }

    /// Swaps the spec at `path`, keeping its request and served-byte counters.
    pub fn replace(&self, path: &str, spec: FileSpec) {
        check_headers(&spec);
        let mut map = lock(&self.shared);
        let entry = map
            .get_mut(&normalize(path))
            .expect("replace: path was never served");
        entry.data = Bytes::from(content(&spec));
        entry.spec = spec;
        entry.drop_spent = false;
    }

    /// Requests received for `path`, including failed ones.
    pub fn requests(&self, path: &str) -> usize {
        lock(&self.shared)
            .get(&normalize(path))
            .map_or(0, |e| e.requests)
    }

    /// Body bytes actually yielded to clients for `path`.
    pub fn bytes_served(&self, path: &str) -> u64 {
        lock(&self.shared)
            .get(&normalize(path))
            .map_or(0, |e| e.served.load(Ordering::Relaxed))
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

enum RangeReq {
    Whole,
    From { start: usize, end: Option<usize> },
}

/// Understands `bytes=a-b` and `bytes=a-`; anything else is treated as no Range.
fn parse_range(spec: &FileSpec, headers: &HeaderMap) -> RangeReq {
    let Some(raw) = headers.get(RANGE).and_then(|v| v.to_str().ok()) else {
        return RangeReq::Whole;
    };
    if !spec.ranges || (spec.ignore_range_after_probe && raw != "bytes=0-0") {
        return RangeReq::Whole;
    }
    let Some((a, b)) = raw.strip_prefix("bytes=").and_then(|r| r.split_once('-')) else {
        return RangeReq::Whole;
    };
    let Ok(start) = a.trim().parse::<usize>() else {
        return RangeReq::Whole;
    };
    let end = match b.trim() {
        "" => None,
        b => match b.parse::<usize>() {
            Ok(end) if end >= start => Some(end),
            _ => return RangeReq::Whole,
        },
    };
    RangeReq::From { start, end }
}

fn empty(status: StatusCode) -> Response<Body> {
    let mut resp = Response::new(Body::empty());
    *resp.status_mut() = status;
    resp
}

async fn handle(
    State(shared): State<Shared>,
    Path(path): Path<String>,
    headers: HeaderMap,
) -> Response<Body> {
    let mut map = lock(&shared);
    let Some(entry) = map.get_mut(&format!("/{path}")) else {
        return empty(StatusCode::NOT_FOUND);
    };
    entry.requests += 1;
    let spec = &entry.spec;
    if let Some(&status) = spec.fail_first.get(entry.requests - 1) {
        return empty(StatusCode::from_u16(status).expect("fail_first status is not valid"));
    }

    let size = spec.size;
    let (status, start, end) = match parse_range(spec, &headers) {
        RangeReq::Whole => (StatusCode::OK, 0, size),
        RangeReq::From { start, .. } if start >= size => {
            let mut resp = empty(StatusCode::RANGE_NOT_SATISFIABLE);
            resp.headers_mut().insert(
                CONTENT_RANGE,
                HeaderValue::from_str(&format!("bytes */{size}")).expect("ascii header"),
            );
            return resp;
        }
        RangeReq::From { start, end } => {
            let last = end.map_or(size - 1, |e| e.min(size - 1));
            (StatusCode::PARTIAL_CONTENT, start, last + 1)
        }
    };

    // Window is [start, end); the drop fires once, in the first window holding the offset.
    let mut stop_at = None;
    if let Some(offset) = spec.drop_once_after {
        if !entry.drop_spent && (start..end).contains(&offset) {
            entry.drop_spent = true;
            stop_at = Some(offset);
        }
    }

    let mut resp = Response::new(Body::from_stream(body_stream(Pacing {
        data: entry.data.clone(),
        pos: start,
        end,
        stop_at,
        rate: spec.bytes_per_sec,
        pacer: entry.pacer.clone(),
        served: entry.served.clone(),
        aborted: false,
    })));
    *resp.status_mut() = status;
    let h = resp.headers_mut();
    h.insert(CONTENT_LENGTH, HeaderValue::from(end - start));
    if status == StatusCode::PARTIAL_CONTENT {
        let range = format!("bytes {start}-{}/{size}", end - 1);
        h.insert(
            CONTENT_RANGE,
            HeaderValue::from_str(&range).expect("ascii header"),
        );
    }
    if spec.ranges {
        h.insert(ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    }
    let content_type = spec
        .content_type
        .as_deref()
        .unwrap_or("application/octet-stream");
    for (name, value) in [
        (CONTENT_TYPE, Some(content_type)),
        (ETAG, spec.etag.as_deref()),
        (LAST_MODIFIED, spec.last_modified.as_deref()),
        (CONTENT_DISPOSITION, spec.content_disposition.as_deref()),
    ] {
        if let Some(value) = value {
            h.insert(
                name,
                HeaderValue::from_str(value).expect("validated on serve"),
            );
        }
    }
    resp
}

struct Pacing {
    data: Bytes,
    pos: usize,
    end: usize,
    stop_at: Option<usize>,
    rate: Option<u64>,
    pacer: Arc<Mutex<Instant>>,
    served: Arc<AtomicU64>,
    aborted: bool,
}

fn body_stream(state: Pacing) -> impl futures_util::Stream<Item = Result<Bytes, std::io::Error>> {
    futures_util::stream::unfold(state, |mut s| async move {
        if s.aborted {
            return None;
        }
        let limit = s.stop_at.unwrap_or(s.end);
        if s.pos >= limit {
            if s.stop_at.is_some() {
                s.aborted = true;
                // Let hyper flush the headers and bytes already sent so the client
                // sees a response that dies mid-body, not a refused request.
                tokio::time::sleep(Duration::from_millis(20)).await;
                let err = std::io::Error::new(
                    std::io::ErrorKind::ConnectionAborted,
                    "testkit: connection dropped",
                );
                return Some((Err(err), s));
            }
            return None;
        }
        let len = CHUNK.min(limit - s.pos);
        if let Some(rate) = s.rate {
            let cost = Duration::from_secs_f64(len as f64 / rate.max(1) as f64);
            let slot = {
                let mut next = s.pacer.lock().unwrap_or_else(|e| e.into_inner());
                let slot = (*next).max(Instant::now());
                *next = slot + cost;
                slot
            };
            tokio::time::sleep_until(slot).await;
        }
        let chunk = s.data.slice(s.pos..s.pos + len);
        s.pos += len;
        s.served.fetch_add(len as u64, Ordering::Relaxed);
        Some((Ok(chunk), s))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    async fn get(url: &str, range: Option<&str>) -> reqwest::Response {
        let client = reqwest::Client::new();
        let mut req = client.get(url);
        if let Some(r) = range {
            req = req.header("range", r);
        }
        req.send().await.unwrap()
    }

    fn header(resp: &reqwest::Response, name: &str) -> String {
        resp.headers()[name].to_str().unwrap().to_owned()
    }

    #[tokio::test]
    async fn probe_returns_206_with_content_range() {
        let server = TestServer::start().await;
        let url = server.serve("/f.bin", FileSpec::new(1000));
        let resp = get(&url, Some("bytes=0-0")).await;
        assert_eq!(resp.status(), 206);
        assert_eq!(header(&resp, "content-range"), "bytes 0-0/1000");
        assert_eq!(header(&resp, "accept-ranges"), "bytes");
        assert_eq!(resp.bytes().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn range_request_returns_slice() {
        let server = TestServer::start().await;
        let spec = FileSpec::new(1000);
        let url = server.serve("/f.bin", spec.clone());
        let resp = get(&url, Some("bytes=100-199")).await;
        assert_eq!(resp.status(), 206);
        assert_eq!(header(&resp, "content-range"), "bytes 100-199/1000");
        assert_eq!(header(&resp, "content-length"), "100");
        assert_eq!(resp.bytes().await.unwrap()[..], content(&spec)[100..200]);
    }

    #[tokio::test]
    async fn open_ended_range_is_clamped_to_size() {
        let server = TestServer::start().await;
        let spec = FileSpec::new(1000);
        let url = server.serve("/f.bin", spec.clone());
        let resp = get(&url, Some("bytes=900-5000")).await;
        assert_eq!(resp.status(), 206);
        assert_eq!(header(&resp, "content-range"), "bytes 900-999/1000");
        assert_eq!(resp.bytes().await.unwrap()[..], content(&spec)[900..]);
        let resp = get(&url, Some("bytes=990-")).await;
        assert_eq!(header(&resp, "content-range"), "bytes 990-999/1000");
    }

    #[tokio::test]
    async fn no_ranges_returns_full_body() {
        let server = TestServer::start().await;
        let mut spec = FileSpec::new(1000);
        spec.ranges = false;
        let url = server.serve("/f.bin", spec);
        let resp = get(&url, Some("bytes=100-199")).await;
        assert_eq!(resp.status(), 200);
        assert!(resp.headers().get("accept-ranges").is_none());
        assert_eq!(resp.bytes().await.unwrap().len(), 1000);
    }

    #[tokio::test]
    async fn plain_get_returns_200_with_length() {
        let server = TestServer::start().await;
        let spec = FileSpec::new(1000);
        let url = server.serve("/f.bin", spec.clone());
        let resp = get(&url, None).await;
        assert_eq!(resp.status(), 200);
        assert_eq!(header(&resp, "content-length"), "1000");
        assert_eq!(header(&resp, "content-type"), "application/octet-stream");
        assert_eq!(resp.bytes().await.unwrap()[..], content(&spec)[..]);
    }

    #[tokio::test]
    async fn ignore_range_after_probe() {
        let server = TestServer::start().await;
        let mut spec = FileSpec::new(1000);
        spec.ignore_range_after_probe = true;
        let url = server.serve("/f.bin", spec);
        let resp = get(&url, Some("bytes=0-0")).await;
        assert_eq!(resp.status(), 206);
        let resp = get(&url, Some("bytes=100-")).await;
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.bytes().await.unwrap().len(), 1000);
    }

    #[tokio::test]
    async fn unsatisfiable_range_returns_416() {
        let server = TestServer::start().await;
        let empty = server.serve("/empty", FileSpec::new(0));
        let resp = get(&empty, Some("bytes=0-0")).await;
        assert_eq!(resp.status(), 416);
        assert_eq!(header(&resp, "content-range"), "bytes */0");
        assert!(resp.bytes().await.unwrap().is_empty());

        let small = server.serve("/small", FileSpec::new(1000));
        let resp = get(&small, Some("bytes=2000-")).await;
        assert_eq!(resp.status(), 416);
        assert_eq!(header(&resp, "content-range"), "bytes */1000");
        assert!(resp.bytes().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn fail_first_statuses_then_ok() {
        let server = TestServer::start().await;
        let mut spec = FileSpec::new(1000);
        spec.fail_first = vec![503, 503];
        let url = server.serve("/f.bin", spec);
        assert_eq!(get(&url, None).await.status(), 503);
        assert_eq!(get(&url, None).await.status(), 503);
        assert_eq!(get(&url, None).await.status(), 200);
        assert_eq!(server.requests("/f.bin"), 3);
    }

    #[tokio::test]
    async fn drop_once_aborts_body() {
        let server = TestServer::start().await;
        let mut spec = FileSpec::new(1000);
        spec.drop_once_after = Some(500);
        let url = server.serve("/f.bin", spec.clone());
        let first = get(&url, None).await;
        match first.bytes().await {
            Err(_) => {}
            Ok(body) => assert!(body.len() < 1000),
        }
        let second = get(&url, None).await;
        assert_eq!(second.status(), 200);
        assert_eq!(second.bytes().await.unwrap()[..], content(&spec)[..]);
    }

    #[tokio::test]
    async fn drop_once_skips_responses_not_crossing_offset() {
        let server = TestServer::start().await;
        let mut spec = FileSpec::new(1000);
        spec.drop_once_after = Some(500);
        let url = server.serve("/f.bin", spec.clone());
        // Window 0..=99 does not contain offset 500, so the drop stays armed.
        let resp = get(&url, Some("bytes=0-99")).await;
        assert_eq!(resp.bytes().await.unwrap()[..], content(&spec)[..100]);
        let resp = get(&url, Some("bytes=400-")).await;
        assert!(resp.bytes().await.is_err());
        let resp = get(&url, Some("bytes=400-")).await;
        assert_eq!(resp.bytes().await.unwrap()[..], content(&spec)[400..]);
    }

    #[tokio::test]
    async fn etag_and_disposition_headers_sent() {
        let server = TestServer::start().await;
        let mut spec = FileSpec::new(10);
        spec.etag = Some("\"abc123\"".into());
        spec.last_modified = Some("Wed, 21 Oct 2015 07:28:00 GMT".into());
        spec.content_disposition = Some("attachment; filename=\"report.pdf\"".into());
        spec.content_type = Some("application/pdf".into());
        let url = server.serve("/f.bin", spec);
        let resp = get(&url, None).await;
        assert_eq!(header(&resp, "etag"), "\"abc123\"");
        assert_eq!(
            header(&resp, "last-modified"),
            "Wed, 21 Oct 2015 07:28:00 GMT"
        );
        assert_eq!(
            header(&resp, "content-disposition"),
            "attachment; filename=\"report.pdf\""
        );
        assert_eq!(header(&resp, "content-type"), "application/pdf");
    }

    #[tokio::test]
    async fn unknown_path_is_404() {
        let server = TestServer::start().await;
        let url = server.serve("/known", FileSpec::new(1));
        let resp = get(&url.replace("known", "other"), None).await;
        assert_eq!(resp.status(), 404);
    }

    #[tokio::test]
    async fn replace_swaps_content_and_keeps_counters() {
        let server = TestServer::start().await;
        let url = server.serve("/f.bin", FileSpec::new(100));
        assert_eq!(get(&url, None).await.bytes().await.unwrap().len(), 100);
        let mut spec = FileSpec::new(200);
        spec.seed = 9;
        server.replace("/f.bin", spec.clone());
        let body = get(&url, None).await.bytes().await.unwrap();
        assert_eq!(body[..], content(&spec)[..]);
        assert_eq!(server.requests("/f.bin"), 2);
        assert_eq!(server.bytes_served("/f.bin"), 300);
    }

    #[tokio::test]
    async fn bytes_served_counts_body_bytes() {
        let server = TestServer::start().await;
        let url = server.serve("/f.bin", FileSpec::new(1000));
        assert_eq!(server.bytes_served("/f.bin"), 0);
        let resp = get(&url, Some("bytes=0-99")).await;
        assert_eq!(resp.bytes().await.unwrap().len(), 100);
        assert_eq!(server.bytes_served("/f.bin"), 100);
        assert_eq!(get(&url, None).await.bytes().await.unwrap().len(), 1000);
        assert_eq!(server.bytes_served("/f.bin"), 1100);
    }

    #[tokio::test]
    async fn bytes_served_counts_only_sent_bytes_on_drop() {
        let server = TestServer::start().await;
        let mut spec = FileSpec::new(1000);
        spec.drop_once_after = Some(500);
        let url = server.serve("/f.bin", spec);
        let _ = get(&url, None).await.bytes().await;
        assert_eq!(server.bytes_served("/f.bin"), 500);
    }

    #[tokio::test]
    async fn throttle_limits_rate() {
        let server = TestServer::start().await;
        let mut spec = FileSpec::new(64 * 1024);
        spec.bytes_per_sec = Some(128 * 1024);
        let url = server.serve("/f.bin", spec);
        let start = Instant::now();
        assert_eq!(
            get(&url, None).await.bytes().await.unwrap().len(),
            64 * 1024
        );
        // 4 chunks of 16 KiB at 128 KiB/s: the last chunk is released at 0.375 s.
        assert!(start.elapsed() >= Duration::from_millis(300));
    }

    #[tokio::test]
    async fn throttle_is_shared_across_connections() {
        let server = TestServer::start().await;
        let mut spec = FileSpec::new(256 * 1024);
        spec.bytes_per_sec = Some(512 * 1024);
        let url = server.serve("/f.bin", spec);
        let start = Instant::now();
        let fetches = (0..4).map(|i| {
            let url = url.clone();
            async move {
                let lo = i * 64 * 1024;
                let range = format!("bytes={}-{}", lo, lo + 64 * 1024 - 1);
                get(&url, Some(&range)).await.bytes().await.unwrap().len()
            }
        });
        let sizes = futures_util::future::join_all(fetches).await;
        assert!(sizes.iter().all(|&n| n == 64 * 1024));
        // 16 chunks at 512 KiB/s: the last is released at 15 * 31.25 ms = 0.469 s.
        assert!(start.elapsed() >= Duration::from_millis(400));
    }

    #[test]
    fn content_is_deterministic_and_seeded() {
        let a = content(&FileSpec::new(100));
        assert_eq!(a, content(&FileSpec::new(100)));
        let mut other = FileSpec::new(100);
        other.seed = 2;
        assert_ne!(a, content(&other));
        let mut zero = FileSpec::new(100);
        zero.seed = 0;
        assert!(content(&zero).iter().any(|&b| b != 0));
        assert_eq!(content(&FileSpec::new(13)).len(), 13);
    }

    #[test]
    fn sha256_hex_matches_known_vector() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
