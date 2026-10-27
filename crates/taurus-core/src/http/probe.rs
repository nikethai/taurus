use reqwest::StatusCode;
use reqwest::header::{
    ACCEPT_ENCODING, CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, ETAG,
    HeaderMap, HeaderName, LAST_MODIFIED, RANGE,
};

use crate::errors::CoreError;
use crate::filename::choose_name;
use crate::http::client::{request_error, status_error};

/// What the server told us about a URL before downloading it.
#[derive(Clone, Debug)]
pub struct Probe {
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
    let response = client
        .get(url)
        .header(RANGE, "bytes=0-0")
        .header(ACCEPT_ENCODING, "identity")
        .send()
        .await
        .map_err(request_error)?;
    let status = response.status();
    let headers = response.headers();
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
    let final_url = response.url().clone();
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
    })
}

fn header(headers: &HeaderMap, name: HeaderName) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
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
