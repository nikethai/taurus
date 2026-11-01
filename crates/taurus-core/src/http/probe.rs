use reqwest::StatusCode;
use reqwest::header::{
    ACCEPT_ENCODING, CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, ETAG,
    HeaderMap, HeaderName, LAST_MODIFIED, RANGE, REFERER,
};

use crate::errors::{CoreError, ErrorKind};
use crate::filename::choose_name;
use crate::http::client::{bot_check_error, request_error, status_error};
use crate::page::{self, MAX_HTML_BYTES, PageVerdict};

/// What the server told us about a URL before downloading it.
#[derive(Clone, Debug)]
pub struct Probe {
    /// The URL requested to obtain this probe: the input URL, or the confirm
    /// form's URL after `resolve` followed one.
    pub requested_url: url::Url,
    /// `requested_url` after redirects.
    pub final_url: url::Url,
    pub size: Option<u64>,
    /// The server honors byte ranges and the size is known, so the file can
    /// be split into segments and resumed.
    pub ranges: bool,
    pub file_name: String,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub content_type: Option<String>,
}

/// `GET` with `Range: bytes=0-0` (never HEAD). Only headers are read.
pub async fn probe(client: &reqwest::Client, url: &str) -> Result<Probe, CoreError> {
    let url = parse_url(url)?;
    let response = send(client, &url, None, true).await?;
    let final_url = response.url().clone();
    from_headers(url, final_url, response.status(), response.headers())
}

/// [`probe`] that recognizes HTML answers (see [`page`]): it follows at most
/// one confirm form and fails every other page with its error. Non-HTML
/// answers are judged by their headers alone, as in `probe`. `referer` is
/// sent with every request.
pub async fn resolve(
    client: &reqwest::Client,
    url: &str,
    referer: Option<&str>,
) -> Result<Probe, CoreError> {
    let mut requested = parse_url(url)?;
    let mut followed_form = false;
    loop {
        let mut response = send(client, &requested, referer, true).await?;
        let content_type = header(response.headers(), CONTENT_TYPE);
        if !page::is_html(content_type.as_deref()) {
            let status = response.status();
            if page::is_challenge_header(response.headers()) {
                return Err(bot_check_error(status));
            }
            let final_url = response.url().clone();
            return from_headers(requested, final_url, status, response.headers());
        }
        if response.status() == StatusCode::PARTIAL_CONTENT {
            // One byte of HTML cannot be judged: read the page without a range.
            response = send(client, &requested, referer, false).await?;
        }
        let status = response.status();
        let headers = response.headers().clone();
        let page_url = response.url().clone();
        let body = read_page(response).await?;
        match page::classify_at(&requested, &page_url, status.as_u16(), &headers, &body) {
            PageVerdict::BotCheck => return Err(bot_check_error(status)),
            PageVerdict::QuotaExceeded => {
                return Err(error(
                    ErrorKind::QuotaExceeded,
                    "the host's download quota for this file is exceeded",
                ));
            }
            _ if !status.is_success() => {
                return from_headers(requested, page_url, status, &headers);
            }
            PageVerdict::ConfirmForm(next) if !followed_form => {
                followed_form = true;
                requested = next;
            }
            PageVerdict::ConfirmForm(_) => {
                return Err(error(
                    ErrorKind::SiteNotSupported,
                    "the confirm page repeated after following its form",
                ));
            }
            PageVerdict::CaptivePortal => {
                return Err(error(
                    ErrorKind::CaptivePortal,
                    "a web page answered instead of the file",
                ));
            }
            PageVerdict::WebPage => {
                return Err(error(
                    ErrorKind::SiteNotSupported,
                    "this link is a web page",
                ));
            }
        }
    }
}

fn parse_url(url: &str) -> Result<url::Url, CoreError> {
    url::Url::parse(url).map_err(|e| error(ErrorKind::Unknown, format!("invalid URL {url:?}: {e}")))
}

async fn send(
    client: &reqwest::Client,
    url: &url::Url,
    referer: Option<&str>,
    ranged: bool,
) -> Result<reqwest::Response, CoreError> {
    let mut request = client.get(url.clone()).header(ACCEPT_ENCODING, "identity");
    if ranged {
        request = request.header(RANGE, "bytes=0-0");
    }
    if let Some(referer) = referer {
        request = request.header(REFERER, referer);
    }
    request.send().await.map_err(request_error)
}

/// At most [`MAX_HTML_BYTES`] of the body, as text.
async fn read_page(mut response: reqwest::Response) -> Result<String, CoreError> {
    let mut body = Vec::new();
    while body.len() < MAX_HTML_BYTES {
        match response.chunk().await.map_err(request_error)? {
            Some(chunk) => body.extend_from_slice(&chunk),
            None => break,
        }
    }
    body.truncate(MAX_HTML_BYTES);
    Ok(String::from_utf8_lossy(&body).into_owned())
}

/// The probe a response's status and headers describe, or its status error.
fn from_headers(
    requested_url: url::Url,
    final_url: url::Url,
    status: StatusCode,
    headers: &HeaderMap,
) -> Result<Probe, CoreError> {
    let content_range = header(headers, CONTENT_RANGE)
        .as_deref()
        .and_then(parse_content_range);
    let (size, ranges) = match status {
        StatusCode::PARTIAL_CONTENT => {
            let size = content_range.and_then(|r| r.total);
            (size, size.is_some())
        }
        // `bytes */N`: the range is past the end, which for `0-0` means an empty file.
        StatusCode::RANGE_NOT_SATISFIABLE => match content_range {
            Some(ContentRange {
                first: None,
                total: Some(size),
            }) => (Some(size), false),
            _ => return Err(status_error(status)),
        },
        s if s.is_success() => {
            let size = header(headers, CONTENT_LENGTH).and_then(|v| v.trim().parse().ok());
            (size, false)
        }
        _ => return Err(status_error(status)),
    };
    let content_type = header(headers, CONTENT_TYPE);
    let file_name = choose_name(
        header(headers, CONTENT_DISPOSITION).as_deref(),
        &final_url,
        content_type.as_deref(),
    );
    Ok(Probe {
        size,
        ranges,
        file_name,
        etag: header(headers, ETAG),
        last_modified: header(headers, LAST_MODIFIED),
        content_type,
        final_url,
        requested_url,
    })
}

fn error(kind: ErrorKind, detail: impl Into<String>) -> CoreError {
    CoreError {
        kind,
        detail: detail.into(),
    }
}

fn header(headers: &HeaderMap, name: HeaderName) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
}

/// How the validators of a response say it is not the file the job was
/// started on: by ETag when both sides have one (a weak `W/` prefix
/// ignored), else by Last-Modified when both have one. ETags that differ
/// while both sides carry the same Last-Modified are taken as the same file:
/// load-balanced origins tag identical bytes per backend (IIS change
/// numbers, Apache inodes). The size is the caller's to compare. `None`
/// when they match or cannot tell.
pub(crate) fn validator_change(
    (old_etag, old_modified): (Option<&str>, Option<&str>),
    (new_etag, new_modified): (Option<&str>, Option<&str>),
) -> Option<String> {
    let same_modified =
        matches!((old_modified, new_modified), (Some(old), Some(new)) if old == new);
    match ((old_etag, new_etag), (old_modified, new_modified)) {
        ((Some(old), Some(new)), _) => (strong(old) != strong(new) && !same_modified)
            .then(|| format!("ETag changed from {old} to {new}")),
        (_, (Some(old), Some(new))) if old != new => {
            Some(format!("Last-Modified changed from {old} to {new}"))
        }
        _ => None,
    }
}

/// The ETag without its weak-validator prefix.
fn strong(etag: &str) -> &str {
    etag.strip_prefix("W/").unwrap_or(etag)
}

/// A parsed `Content-Range: bytes <first>-<last>/<total>` (either side may be `*`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ContentRange {
    pub first: Option<u64>,
    pub total: Option<u64>,
}

pub(crate) fn parse_content_range(value: &str) -> Option<ContentRange> {
    let rest = value.trim().strip_prefix("bytes")?.trim_start();
    let (range, total) = rest.split_once('/')?;
    let total = match total.trim() {
        "*" => None,
        t => Some(t.parse().ok()?),
    };
    let first = match range.trim() {
        "*" => None,
        r => Some(r.split_once('-')?.0.trim().parse().ok()?),
    };
    Some(ContentRange { first, total })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cr(first: Option<u64>, total: Option<u64>) -> Option<ContentRange> {
        Some(ContentRange { first, total })
    }

    #[test]
    fn validators_compare_etag_first_then_last_modified() {
        let (v1, v2) = (Some("\"v1\""), Some("\"v2\""));
        let (monday, tuesday) = (Some("Mon"), Some("Tue"));
        // Differing ETags with differing or missing Last-Modified: changed.
        assert_eq!(
            validator_change((v1, monday), (v2, tuesday)).as_deref(),
            Some("ETag changed from \"v1\" to \"v2\"")
        );
        assert!(validator_change((v1, monday), (v2, None)).is_some());
        assert!(validator_change((v1, None), (v2, monday)).is_some());
        assert!(validator_change((v1, None), (v2, None)).is_some());
        // Matching ETags win over a differing Last-Modified.
        assert_eq!(validator_change((v1, monday), (v1, tuesday)), None);
        // Without an ETag on both sides, Last-Modified decides.
        assert_eq!(
            validator_change((v1, monday), (None, tuesday)).as_deref(),
            Some("Last-Modified changed from Mon to Tue")
        );
        assert_eq!(validator_change((None, monday), (None, monday)), None);
        assert_eq!(validator_change((None, None), (v2, tuesday)), None);
    }

    #[test]
    fn per_backend_etags_with_one_last_modified_are_one_file() {
        let modified = Some("Tue, 01 Jan 2030 00:00:00 GMT");
        assert_eq!(
            validator_change(
                (Some("\"5f2a:12\""), modified),
                (Some("\"5f2a:13\""), modified)
            ),
            None
        );
    }

    #[test]
    fn a_weak_etag_matches_its_strong_form() {
        let (weak, strong) = (Some("W/\"x\""), Some("\"x\""));
        assert_eq!(validator_change((weak, None), (strong, None)), None);
        assert_eq!(validator_change((strong, None), (weak, None)), None);
        assert!(validator_change((Some("W/\"x\""), None), (Some("W/\"y\""), None)).is_some());
    }

    #[test]
    fn content_range_forms() {
        assert_eq!(
            parse_content_range("bytes 0-0/1000"),
            cr(Some(0), Some(1000))
        );
        assert_eq!(parse_content_range("bytes 500-999/*"), cr(Some(500), None));
        assert_eq!(parse_content_range("bytes */42"), cr(None, Some(42)));
        assert_eq!(parse_content_range("items 0-1/2"), None);
        assert_eq!(parse_content_range("bytes 0-1"), None);
        assert_eq!(parse_content_range("bytes x-1/2"), None);
    }
}
