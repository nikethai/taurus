use crate::errors::{CoreError, ErrorKind, classify_reqwest, classify_status};

pub(crate) const USER_AGENT: &str = concat!(
    "Hyades/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/nikethai/taurus)"
);

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
                "Hyades/{} (+https://github.com/nikethai/taurus)",
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
