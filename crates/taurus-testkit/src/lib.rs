//! Test support for Taurus: a local HTTP server with scriptable behavior.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::header::{
    ACCEPT_RANGES, CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, ETAG,
    HeaderMap, HeaderName, HeaderValue, LAST_MODIFIED, LOCATION, RANGE,
};
use axum::http::{Response, StatusCode};
use axum::routing::get;
use bytes::Bytes;
use sha2::{Digest, Sha256};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio::time::Instant;

const CHUNK: usize = 16 * 1024;
const HTML: &str = "text/html; charset=utf-8";
const BLOCKED_BODY: &str = "<html><body>blocked</body></html>";

/// Every request after the first `after_requests` answers `status` with a small
/// HTML body, like a host that starts blocking or challenging a client.
///
/// The response carries `content-type: text/html; charset=utf-8` unless `headers`
/// names `content-type` itself; other `headers` are added on top.
#[derive(Clone, Debug)]
pub struct FailAfter {
    pub after_requests: usize,
    pub status: u16,
    pub headers: Vec<(String, String)>,
}

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
    /// Request number k (1-based) answers `fail_first[k - 1]` while k <= len;
    /// a `0` entry serves that request normally.
    pub fail_first: Vec<u16>,
    /// A request arriving while this many bodies are already in flight answers
    /// `over_limit_status` with an empty body.
    pub max_concurrent: Option<usize>,
    pub over_limit_status: u16,
    pub fail_after: Option<FailAfter>,
    /// Added to every successful response.
    pub extra_headers: Vec<(String, String)>,
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
            max_concurrent: None,
            over_limit_status: 429,
            fail_after: None,
            extra_headers: Vec::new(),
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

/// One served path: a request counter and header capture shared by every kind.
struct Entry {
    requests: usize,
    last_headers: Vec<(String, String)>,
    kind: Kind,
}

enum Kind {
    File(Box<FileEntry>),
    Page(Page),
    /// `location` values, cycled by request number.
    Redirect(Vec<String>),
}

struct FileEntry {
    spec: FileSpec,
    data: Bytes,
    served: Arc<AtomicU64>,
    drop_spent: bool,
    /// Earliest release time of the next chunk; shared by every response.
    pacer: Arc<Mutex<Instant>>,
    flight: Arc<Flight>,
    rejections: usize,
}

struct Page {
    status: u16,
    headers: Vec<(String, String)>,
    html: String,
}

/// Bodies currently being sent for a path, and the most seen at once.
#[derive(Default)]
struct Flight {
    current: AtomicUsize,
    peak: AtomicUsize,
}

/// Counts one body in flight; released when the body stream is dropped.
struct FlightGuard(Arc<Flight>);

impl FlightGuard {
    fn enter(flight: &Arc<Flight>) -> Self {
        let now = flight.current.fetch_add(1, Ordering::SeqCst) + 1;
        flight.peak.fetch_max(now, Ordering::SeqCst);
        Self(flight.clone())
    }
}

impl Drop for FlightGuard {
    fn drop(&mut self) {
        self.0.current.fetch_sub(1, Ordering::SeqCst);
    }
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

/// Inserts each pair, so a later pair (or an existing header) of the same name is replaced.
fn apply_headers(map: &mut HeaderMap, pairs: &[(String, String)]) {
    for (name, value) in pairs {
        map.insert(
            HeaderName::from_bytes(name.as_bytes())
                .expect("testkit header name is not a valid HTTP header name"),
            HeaderValue::from_str(value).expect("testkit header value is not a valid HTTP header"),
        );
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
    apply_headers(&mut HeaderMap::new(), &spec.extra_headers);
    if let Some(fail) = &spec.fail_after {
        apply_headers(&mut HeaderMap::new(), &fail.headers);
        StatusCode::from_u16(fail.status).expect("fail_after status is not valid");
    }
    StatusCode::from_u16(spec.over_limit_status).expect("over_limit_status is not valid");
    for &status in spec.fail_first.iter().filter(|&&s| s != 0) {
        StatusCode::from_u16(status).expect("fail_first status is not valid");
    }
}

fn owned_pairs(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .map(|&(n, v)| (n.to_owned(), v.to_owned()))
        .collect()
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

    /// Registers `kind` at `path` with fresh counters and returns the full URL.
    fn register(&self, path: &str, kind: Kind) -> String {
        let path = normalize(path);
        let entry = Entry {
            requests: 0,
            last_headers: Vec::new(),
            kind,
        };
        lock(&self.shared).insert(path.clone(), entry);
        format!("{}{path}", self.base)
    }

    /// Registers `spec` at `path` and returns the full URL.
    pub fn serve(&self, path: &str, spec: FileSpec) -> String {
        check_headers(&spec);
        self.register(
            path,
            Kind::File(Box::new(FileEntry {
                data: Bytes::from(content(&spec)),
                spec,
                served: Arc::new(AtomicU64::new(0)),
                drop_spent: false,
                pacer: Arc::new(Mutex::new(Instant::now())),
                flight: Arc::default(),
                rejections: 0,
            })),
        )
    }

    /// Registers a fixed answer at `path` and returns the full URL: `status`, then
    /// `headers`, then `html`, with `content-type: text/html; charset=utf-8` unless
    /// `headers` names `content-type`. Range is ignored.
    pub fn serve_page(
        &self,
        path: &str,
        status: u16,
        headers: &[(&str, &str)],
        html: &str,
    ) -> String {
        let headers = owned_pairs(headers);
        apply_headers(&mut HeaderMap::new(), &headers);
        StatusCode::from_u16(status).expect("serve_page status is not valid");
        self.register(
            path,
            Kind::Page(Page {
                status,
                headers,
                html: html.to_owned(),
            }),
        )
    }

    /// Registers `path` answering 302 with `location: targets[n % targets.len()]` on
    /// its n-th request (0-based). Targets are sent exactly as given, e.g. "/s1".
    pub fn redirect_cycle(&self, path: &str, targets: &[&str]) -> String {
        assert!(!targets.is_empty(), "redirect_cycle needs a target");
        for target in targets {
            HeaderValue::from_str(target).expect("redirect target is not a valid HTTP header");
        }
        self.register(
            path,
            Kind::Redirect(targets.iter().map(|&t| t.to_owned()).collect()),
        )
    }

    /// Swaps the spec at `path`, keeping its counters.
    pub fn replace(&self, path: &str, spec: FileSpec) {
        check_headers(&spec);
        let mut map = lock(&self.shared);
        let entry = map
            .get_mut(&normalize(path))
            .expect("replace: path was never served");
        let Kind::File(file) = &mut entry.kind else {
            panic!("replace: path is not a file");
        };
        file.data = Bytes::from(content(&spec));
        file.spec = spec;
        file.drop_spent = false;
    }

    /// Requests received for `path`, including failed ones.
    pub fn requests(&self, path: &str) -> usize {
        lock(&self.shared)
            .get(&normalize(path))
            .map_or(0, |e| e.requests)
    }

    /// Body bytes actually yielded to clients for `path`.
    pub fn bytes_served(&self, path: &str) -> u64 {
        self.with_file(path, 0, |f| f.served.load(Ordering::Relaxed))
    }

    /// The most bodies in flight at once for `path`; a body counts until its
    /// stream completes, aborts or is dropped.
    pub fn peak_concurrency(&self, path: &str) -> usize {
        self.with_file(path, 0, |f| f.flight.peak.load(Ordering::SeqCst))
    }

    /// Requests for `path` refused with `over_limit_status` by `max_concurrent`.
    pub fn over_limit_rejections(&self, path: &str) -> usize {
        self.with_file(path, 0, |f| f.rejections)
    }

    /// Headers of the most recent request to `path` (file, page or redirect),
    /// names lower-cased, in request order; empty before any request.
    pub fn last_request_headers(&self, path: &str) -> Vec<(String, String)> {
        lock(&self.shared)
            .get(&normalize(path))
            .map_or_else(Vec::new, |e| e.last_headers.clone())
    }

    fn with_file<T>(&self, path: &str, default: T, f: impl FnOnce(&FileEntry) -> T) -> T {
        match lock(&self.shared).get(&normalize(path)).map(|e| &e.kind) {
            Some(Kind::File(file)) => f(file),
            _ => default,
        }
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

/// A whole HTML answer: `headers` are applied over the default content type.
fn html_response(status: u16, headers: &[(String, String)], html: &str) -> Response<Body> {
    let mut resp = Response::new(Body::from(html.to_owned()));
    *resp.status_mut() = StatusCode::from_u16(status).expect("validated on registration");
    let h = resp.headers_mut();
    h.insert(CONTENT_TYPE, HeaderValue::from_static(HTML));
    apply_headers(h, headers);
    h.insert(CONTENT_LENGTH, HeaderValue::from(html.len()));
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
    let number = entry.requests;
    entry.last_headers = headers
        .iter()
        .map(|(name, value)| {
            (
                name.as_str().to_owned(),
                String::from_utf8_lossy(value.as_bytes()).into_owned(),
            )
        })
        .collect();
    match &mut entry.kind {
        Kind::File(file) => file_response(file, number, &headers),
        Kind::Page(page) => html_response(page.status, &page.headers, &page.html),
        Kind::Redirect(targets) => {
            let mut resp = empty(StatusCode::FOUND);
            let target = &targets[(number - 1) % targets.len()];
            resp.headers_mut().insert(
                LOCATION,
                HeaderValue::from_str(target).expect("validated on registration"),
            );
            resp
        }
    }
}

/// Request `number` (1-based) for a file: `fail_first`, `fail_after`,
/// `max_concurrent`, then the normal response.
fn file_response(entry: &mut FileEntry, number: usize, headers: &HeaderMap) -> Response<Body> {
    let spec = &entry.spec;
    if let Some(&status) = spec.fail_first.get(number - 1)
        && status != 0
    {
        return empty(StatusCode::from_u16(status).expect("validated on serve"));
    }
    if let Some(fail) = &spec.fail_after
        && number > fail.after_requests
    {
        return html_response(fail.status, &fail.headers, BLOCKED_BODY);
    }
    if let Some(max) = spec.max_concurrent
        && entry.flight.current.load(Ordering::SeqCst) >= max
    {
        entry.rejections += 1;
        return empty(StatusCode::from_u16(spec.over_limit_status).expect("validated on serve"));
    }

    let size = spec.size;
    let (status, start, end) = match parse_range(spec, headers) {
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
    if let Some(offset) = spec.drop_once_after
        && !entry.drop_spent
        && (start..end).contains(&offset)
    {
        entry.drop_spent = true;
        stop_at = Some(offset);
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
        _guard: FlightGuard::enter(&entry.flight),
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
    apply_headers(h, &spec.extra_headers);
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
    /// Held for the life of the stream, so the slot frees when the body is dropped.
    _guard: FlightGuard,
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

    fn no_redirects() -> reqwest::Client {
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap()
    }

    #[tokio::test]
    async fn max_concurrent_rejects_extra() {
        let server = TestServer::start().await;
        let mut spec = FileSpec::new(64 * 1024);
        spec.bytes_per_sec = Some(16 * 1024);
        spec.max_concurrent = Some(1);
        let url = server.serve("/f.bin", spec);
        // Headers arrive with the first chunk; the rest of the body stays in flight.
        let first = get(&url, None).await;
        assert_eq!(first.status(), 200);
        let second = get(&url, None).await;
        assert_eq!(second.status(), 429);
        assert!(second.bytes().await.unwrap().is_empty());
        assert_eq!(server.over_limit_rejections("/f.bin"), 1);
        assert_eq!(server.peak_concurrency("/f.bin"), 1);
        assert_eq!(server.requests("/f.bin"), 2);
    }

    #[tokio::test]
    async fn over_limit_status_is_configurable() {
        let server = TestServer::start().await;
        let mut spec = FileSpec::new(64 * 1024);
        spec.bytes_per_sec = Some(16 * 1024);
        spec.max_concurrent = Some(1);
        spec.over_limit_status = 403;
        let url = server.serve("/f.bin", spec);
        let _first = get(&url, None).await;
        assert_eq!(get(&url, None).await.status(), 403);
    }

    #[tokio::test]
    async fn finished_responses_release_their_slot() {
        let server = TestServer::start().await;
        let mut spec = FileSpec::new(1000);
        spec.max_concurrent = Some(1);
        let url = server.serve("/f.bin", spec);
        for _ in 0..5 {
            let resp = get(&url, None).await;
            assert_eq!(resp.status(), 200);
            assert_eq!(resp.bytes().await.unwrap().len(), 1000);
        }
        assert_eq!(server.over_limit_rejections("/f.bin"), 0);
        assert_eq!(server.peak_concurrency("/f.bin"), 1);
    }

    #[tokio::test]
    async fn peak_concurrency_tracks_simultaneous_bodies() {
        let server = TestServer::start().await;
        let mut spec = FileSpec::new(64 * 1024);
        spec.bytes_per_sec = Some(256 * 1024);
        let url = server.serve("/f.bin", spec);
        let fetches = (0..3).map(|_| {
            let url = url.clone();
            async move { get(&url, None).await.bytes().await.unwrap().len() }
        });
        futures_util::future::join_all(fetches).await;
        assert_eq!(server.peak_concurrency("/f.bin"), 3);
        assert_eq!(server.peak_concurrency("/unknown"), 0);
    }

    #[tokio::test]
    async fn fail_after_switches_status_and_headers() {
        let server = TestServer::start().await;
        let mut spec = FileSpec::new(1000);
        spec.fail_after = Some(FailAfter {
            after_requests: 2,
            status: 403,
            headers: vec![("cf-mitigated".into(), "challenge".into())],
        });
        let url = server.serve("/f.bin", spec);
        assert_eq!(get(&url, None).await.status(), 200);
        assert_eq!(get(&url, None).await.status(), 200);
        let resp = get(&url, None).await;
        assert_eq!(resp.status(), 403);
        assert_eq!(header(&resp, "cf-mitigated"), "challenge");
        assert_eq!(header(&resp, "content-type"), "text/html; charset=utf-8");
        let body = resp.bytes().await.unwrap();
        assert_eq!(&body[..], b"<html><body>blocked</body></html>");
        assert_eq!(server.requests("/f.bin"), 3);
    }

    #[tokio::test]
    async fn fail_after_header_replaces_default_content_type() {
        let server = TestServer::start().await;
        let mut spec = FileSpec::new(1000);
        spec.fail_after = Some(FailAfter {
            after_requests: 0,
            status: 200,
            headers: vec![("content-type".into(), "text/plain".into())],
        });
        let url = server.serve("/f.bin", spec);
        let resp = get(&url, None).await;
        assert_eq!(resp.status(), 200);
        assert_eq!(header(&resp, "content-type"), "text/plain");
        assert_eq!(header(&resp, "content-length"), "33");
    }

    #[tokio::test]
    async fn fail_first_zero_serves_normally() {
        let server = TestServer::start().await;
        let mut spec = FileSpec::new(1000);
        spec.fail_first = vec![0, 503, 503];
        let url = server.serve("/f.bin", spec);
        assert_eq!(get(&url, None).await.status(), 200);
        assert_eq!(get(&url, None).await.status(), 503);
        assert_eq!(get(&url, None).await.status(), 503);
        assert_eq!(get(&url, None).await.status(), 200);
    }

    #[tokio::test]
    async fn fail_first_runs_before_fail_after() {
        let server = TestServer::start().await;
        let mut spec = FileSpec::new(1000);
        spec.fail_first = vec![503];
        spec.fail_after = Some(FailAfter {
            after_requests: 0,
            status: 403,
            headers: Vec::new(),
        });
        let url = server.serve("/f.bin", spec);
        assert_eq!(get(&url, None).await.status(), 503);
        assert_eq!(get(&url, None).await.status(), 403);
    }

    #[tokio::test]
    async fn extra_headers_added_to_successful_responses() {
        let server = TestServer::start().await;
        let mut spec = FileSpec::new(10);
        spec.extra_headers = vec![("x-served-by".into(), "testkit".into())];
        let url = server.serve("/f.bin", spec);
        let resp = get(&url, Some("bytes=0-0")).await;
        assert_eq!(resp.status(), 206);
        assert_eq!(header(&resp, "x-served-by"), "testkit");
    }

    #[tokio::test]
    async fn serve_page_returns_html() {
        let server = TestServer::start().await;
        let url = server.serve_page(
            "/login",
            511,
            &[("x-portal", "yes")],
            "<html>sign in</html>",
        );
        assert!(url.ends_with("/login"));
        // Range is ignored; the page is always answered whole.
        let resp = get(&url, Some("bytes=0-0")).await;
        assert_eq!(resp.status(), 511);
        assert_eq!(header(&resp, "content-type"), "text/html; charset=utf-8");
        assert_eq!(header(&resp, "x-portal"), "yes");
        assert_eq!(resp.text().await.unwrap(), "<html>sign in</html>");
        assert_eq!(server.requests("/login"), 1);
    }

    #[tokio::test]
    async fn serve_page_headers_can_override_content_type() {
        let server = TestServer::start().await;
        let url = server.serve_page("/p", 200, &[("content-type", "text/plain")], "hi");
        let resp = get(&url, None).await;
        assert_eq!(header(&resp, "content-type"), "text/plain");
    }

    #[tokio::test]
    async fn redirect_cycle_rotates() {
        let server = TestServer::start().await;
        let url = server.redirect_cycle("/r", &["/s1", "/s2"]);
        let client = no_redirects();
        let mut seen = Vec::new();
        for _ in 0..3 {
            let resp = client.get(&url).send().await.unwrap();
            assert_eq!(resp.status(), 302);
            seen.push(header(&resp, "location"));
        }
        assert_eq!(seen, ["/s1", "/s2", "/s1"]);
        assert_eq!(server.requests("/r"), 3);
    }

    #[tokio::test]
    async fn redirect_cycle_is_followed_by_clients() {
        let server = TestServer::start().await;
        let spec = FileSpec::new(100);
        server.serve("/s1", spec.clone());
        let url = server.redirect_cycle("/r", &["/s1"]);
        let resp = get(&url, None).await;
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.bytes().await.unwrap()[..], content(&spec)[..]);
    }

    #[tokio::test]
    async fn records_request_headers() {
        let server = TestServer::start().await;
        let file = server.serve("/f.bin", FileSpec::new(10));
        let page = server.serve_page("/p", 200, &[], "x");
        let redirect = server.redirect_cycle("/r", &["/p"]);
        assert!(server.last_request_headers("/f.bin").is_empty());
        let client = no_redirects();
        for url in [&file, &page, &redirect] {
            client
                .get(url)
                .header("Referer", "https://a.example/")
                .send()
                .await
                .unwrap();
        }
        for path in ["/f.bin", "/p", "/r"] {
            let headers = server.last_request_headers(path);
            assert!(headers.contains(&("referer".to_owned(), "https://a.example/".to_owned())));
            assert!(headers.iter().any(|(n, _)| n == "host"));
        }
        assert!(server.last_request_headers("/unknown").is_empty());
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
