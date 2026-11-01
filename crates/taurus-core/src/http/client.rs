use std::time::Duration;

use reqwest::header::{ACCEPT_ENCODING, HeaderMap, HeaderValue};

use crate::errors::{CoreError, ErrorKind, classify_reqwest, classify_status};

const USER_AGENT: &str = concat!(
    "Taurus/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/nikethai/taurus)"
);

/// The one client every download uses: stable User-Agent, no content
/// encoding (byte offsets must match the file), system proxy settings, no
/// automatic Referer.
pub fn build_client() -> reqwest::Client {
    let mut headers = HeaderMap::new();
    headers.insert(ACCEPT_ENCODING, HeaderValue::from_static("identity"));
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .default_headers(headers)
        .connect_timeout(Duration::from_secs(20))
        .read_timeout(Duration::from_secs(60))
        .redirect(reqwest::redirect::Policy::limited(10))
        // A redirect keeps the job's Referer (as browsers do) instead of
        // naming the redirecting URL.
        .referer(false)
        .build()
        .expect("static HTTP client configuration is valid")
}

/// Classifies a transport error, keeping its cause chain in the detail.
pub(crate) fn request_error(e: reqwest::Error) -> CoreError {
    let mut detail = e.to_string();
    let mut source = std::error::Error::source(&e);
    while let Some(cause) = source {
        detail.push_str(": ");
        detail.push_str(&cause.to_string());
        source = cause.source();
    }
    CoreError {
        kind: classify_reqwest(&e),
        detail,
    }
}

pub(crate) fn status_error(status: reqwest::StatusCode) -> CoreError {
    CoreError {
        kind: classify_status(status.as_u16()),
        detail: format!("server answered HTTP {status}"),
    }
}

/// A bot check is never solved or retried: the job stops here.
pub(crate) fn bot_check_error(status: reqwest::StatusCode) -> CoreError {
    CoreError {
        kind: ErrorKind::BlockedByBotCheck,
        detail: format!("server answered HTTP {status} with a bot check"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_agent_is_stable() {
        assert_eq!(
            USER_AGENT,
            format!(
                "Taurus/{} (+https://github.com/nikethai/taurus)",
                env!("CARGO_PKG_VERSION")
            )
        );
    }

    #[test]
    fn status_error_maps_kind() {
        let e = status_error(reqwest::StatusCode::NOT_FOUND);
        assert_eq!(e.kind, crate::errors::ErrorKind::LinkExpired);
        assert_eq!(e.detail, "server answered HTTP 404 Not Found");
    }
}
