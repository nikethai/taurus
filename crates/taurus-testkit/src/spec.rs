//! What a served file is and does ([`FileSpec`]), and its bytes.

use axum::http::{HeaderMap, HeaderValue, StatusCode};
use sha2::{Digest, Sha256};

use crate::apply_headers;

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
    /// `(user, password)`: every request without this HTTP Basic
    /// `Authorization` answers 401 with `WWW-Authenticate: Basic
    /// realm="test"` (checked before anything else).
    pub basic_auth: Option<(String, String)>,
    /// Every request waits this long before it is answered at all
    /// (headers too), like a slow server.
    pub delay: Option<std::time::Duration>,
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
            basic_auth: None,
            delay: None,
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

/// Panics on a header value or status the server could not send.
pub(crate) fn check_headers(spec: &FileSpec) {
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
