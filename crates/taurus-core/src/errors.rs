use std::time::Duration;

const BACKOFF_BASE_MS: u64 = 500;
const BACKOFF_CAP_MS: u64 = 30_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum ErrorKind {
    ConnectionLost,
    Timeout,
    LinkExpired,
    NeedsSignIn,
    BlockedByBotCheck,
    CaptivePortal,
    RateLimited,
    QuotaExceeded,
    DiskFull,
    FolderUnavailable,
    NoPermission,
    FileTooLargeForDisk,
    FileChangedOnServer,
    ToolMissing,
    ToolFailed,
    SiteNotSupported,
    ChecksumMismatch,
    Unknown,
}

impl ErrorKind {
    /// Whether retrying the same request later can plausibly succeed.
    pub fn is_transient(self) -> bool {
        matches!(
            self,
            Self::ConnectionLost | Self::Timeout | Self::RateLimited
        )
    }

    /// Stable snake_case name, stored in the database.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ConnectionLost => "connection_lost",
            Self::Timeout => "timeout",
            Self::LinkExpired => "link_expired",
            Self::NeedsSignIn => "needs_sign_in",
            Self::BlockedByBotCheck => "blocked_by_bot_check",
            Self::CaptivePortal => "captive_portal",
            Self::RateLimited => "rate_limited",
            Self::QuotaExceeded => "quota_exceeded",
            Self::DiskFull => "disk_full",
            Self::FolderUnavailable => "folder_unavailable",
            Self::NoPermission => "no_permission",
            Self::FileTooLargeForDisk => "file_too_large_for_disk",
            Self::FileChangedOnServer => "file_changed_on_server",
            Self::ToolMissing => "tool_missing",
            Self::ToolFailed => "tool_failed",
            Self::SiteNotSupported => "site_not_supported",
            Self::ChecksumMismatch => "checksum_mismatch",
            Self::Unknown => "unknown",
        }
    }

    // Infallible and falls back to Unknown, so it cannot be a `FromStr` impl.
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> ErrorKind {
        match s {
            "connection_lost" => Self::ConnectionLost,
            "timeout" => Self::Timeout,
            "link_expired" => Self::LinkExpired,
            "needs_sign_in" => Self::NeedsSignIn,
            "blocked_by_bot_check" => Self::BlockedByBotCheck,
            "captive_portal" => Self::CaptivePortal,
            "rate_limited" => Self::RateLimited,
            "quota_exceeded" => Self::QuotaExceeded,
            "disk_full" => Self::DiskFull,
            "folder_unavailable" => Self::FolderUnavailable,
            "no_permission" => Self::NoPermission,
            "file_too_large_for_disk" => Self::FileTooLargeForDisk,
            "file_changed_on_server" => Self::FileChangedOnServer,
            "tool_missing" => Self::ToolMissing,
            "tool_failed" => Self::ToolFailed,
            "site_not_supported" => Self::SiteNotSupported,
            "checksum_mismatch" => Self::ChecksumMismatch,
            _ => Self::Unknown,
        }
    }
}

#[derive(Clone, Debug, thiserror::Error)]
#[error("{kind:?}: {detail}")]
pub struct CoreError {
    pub kind: ErrorKind,
    pub detail: String,
}

pub fn classify_status(status: u16) -> ErrorKind {
    match status {
        401 => ErrorKind::NeedsSignIn,
        403 | 404 | 410 => ErrorKind::LinkExpired,
        429 | 503 => ErrorKind::RateLimited,
        500..=599 => ErrorKind::ConnectionLost,
        _ => ErrorKind::Unknown,
    }
}

pub fn classify_reqwest(e: &reqwest::Error) -> ErrorKind {
    if e.is_timeout() {
        ErrorKind::Timeout
    } else if let Some(status) = e.status() {
        classify_status(status.as_u16())
    } else if e.is_connect() || e.is_body() || e.is_request() || e.is_decode() {
        // Bodies are always requested with identity encoding, so a decode
        // error is reqwest's wrapper around a body read that failed mid-stream.
        ErrorKind::ConnectionLost
    } else {
        ErrorKind::Unknown
    }
}

pub fn classify_io(e: &std::io::Error) -> ErrorKind {
    use std::io::ErrorKind as Io;
    match e.kind() {
        Io::StorageFull => ErrorKind::DiskFull,
        Io::PermissionDenied => ErrorKind::NoPermission,
        Io::NotFound => ErrorKind::FolderUnavailable,
        Io::FileTooLarge => ErrorKind::FileTooLargeForDisk,
        _ => ErrorKind::Unknown,
    }
}

/// Delay before retry number `attempt` (0-based): 500 ms doubling per attempt,
/// capped at 30 s, with +-20% jitter; the result never exceeds 30 s.
pub fn backoff_delay(attempt: u32) -> Duration {
    backoff_delay_with(
        attempt,
        Duration::from_millis(BACKOFF_BASE_MS),
        Duration::from_millis(BACKOFF_CAP_MS),
    )
}

/// Like [`backoff_delay`] with a chosen `base` and `cap`.
pub fn backoff_delay_with(attempt: u32, base: Duration, cap: Duration) -> Duration {
    let factor = 1u64.checked_shl(attempt).unwrap_or(u64::MAX);
    let cap_ms = u64::try_from(cap.as_millis()).unwrap_or(u64::MAX);
    let base_ms = u64::try_from(base.as_millis()).unwrap_or(u64::MAX);
    let step_ms = base_ms.saturating_mul(factor).min(cap_ms);
    let jittered_ms = step_ms.saturating_mul(fastrand::u64(80..=120)) / 100;
    Duration::from_millis(jittered_ms.min(cap_ms))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;
    use std::time::Duration;

    const ALL: [ErrorKind; 18] = [
        ErrorKind::ConnectionLost,
        ErrorKind::Timeout,
        ErrorKind::LinkExpired,
        ErrorKind::NeedsSignIn,
        ErrorKind::BlockedByBotCheck,
        ErrorKind::CaptivePortal,
        ErrorKind::RateLimited,
        ErrorKind::QuotaExceeded,
        ErrorKind::DiskFull,
        ErrorKind::FolderUnavailable,
        ErrorKind::NoPermission,
        ErrorKind::FileTooLargeForDisk,
        ErrorKind::FileChangedOnServer,
        ErrorKind::ToolMissing,
        ErrorKind::ToolFailed,
        ErrorKind::SiteNotSupported,
        ErrorKind::ChecksumMismatch,
        ErrorKind::Unknown,
    ];

    #[test]
    fn status_mapping() {
        assert_eq!(classify_status(401), ErrorKind::NeedsSignIn);
        assert_eq!(classify_status(403), ErrorKind::LinkExpired);
        assert_eq!(classify_status(404), ErrorKind::LinkExpired);
        assert_eq!(classify_status(410), ErrorKind::LinkExpired);
        assert_eq!(classify_status(429), ErrorKind::RateLimited);
        assert_eq!(classify_status(503), ErrorKind::RateLimited);
        assert_eq!(classify_status(500), ErrorKind::ConnectionLost);
        assert_eq!(classify_status(502), ErrorKind::ConnectionLost);
        assert_eq!(classify_status(418), ErrorKind::Unknown);
    }

    #[test]
    fn io_mapping() {
        let k = |kind| classify_io(&io::Error::from(kind));
        assert_eq!(k(io::ErrorKind::StorageFull), ErrorKind::DiskFull);
        assert_eq!(k(io::ErrorKind::PermissionDenied), ErrorKind::NoPermission);
        assert_eq!(k(io::ErrorKind::NotFound), ErrorKind::FolderUnavailable);
        assert_eq!(
            k(io::ErrorKind::FileTooLarge),
            ErrorKind::FileTooLargeForDisk
        );
        assert_eq!(k(io::ErrorKind::Other), ErrorKind::Unknown);
    }

    #[test]
    fn transient_kinds() {
        for kind in ALL {
            let expected = matches!(
                kind,
                ErrorKind::ConnectionLost | ErrorKind::Timeout | ErrorKind::RateLimited
            );
            assert_eq!(kind.is_transient(), expected, "{kind:?}");
        }
    }

    #[test]
    fn kind_string_roundtrip() {
        for kind in ALL {
            assert_eq!(ErrorKind::from_str(kind.as_str()), kind);
        }
        assert_eq!(ErrorKind::from_str("nope"), ErrorKind::Unknown);
        assert_eq!(ErrorKind::ConnectionLost.as_str(), "connection_lost");
        assert_eq!(
            ErrorKind::BlockedByBotCheck.as_str(),
            "blocked_by_bot_check"
        );
        assert_eq!(
            ErrorKind::FileTooLargeForDisk.as_str(),
            "file_too_large_for_disk"
        );
    }

    #[test]
    fn checksum_mismatch_kind() {
        let kind = ErrorKind::ChecksumMismatch;
        assert_eq!(kind.as_str(), "checksum_mismatch");
        assert_eq!(ErrorKind::from_str("checksum_mismatch"), kind);
        assert!(!kind.is_transient());
    }

    #[test]
    fn backoff_with_respects_base_and_cap() {
        let base = Duration::from_millis(10);
        let cap = Duration::from_millis(50);
        for _ in 0..200 {
            let d0 = backoff_delay_with(0, base, cap);
            assert!((Duration::from_millis(8)..=Duration::from_millis(12)).contains(&d0));
            assert!(backoff_delay_with(10, base, cap) <= cap);
            assert!(backoff_delay_with(u32::MAX, base, cap) <= cap);
        }
    }

    #[test]
    fn core_error_display() {
        let e = CoreError {
            kind: ErrorKind::DiskFull,
            detail: "no space".into(),
        };
        assert_eq!(e.to_string(), "DiskFull: no space");
    }

    #[test]
    fn backoff_grows_and_caps() {
        for _ in 0..200 {
            let d0 = backoff_delay(0);
            assert!((Duration::from_millis(400)..=Duration::from_millis(600)).contains(&d0));
            let d1 = backoff_delay(1);
            assert!((Duration::from_millis(800)..=Duration::from_millis(1200)).contains(&d1));
            assert!(backoff_delay(20) <= Duration::from_secs(30));
            assert!(backoff_delay(u32::MAX) <= Duration::from_secs(30));
            assert!(backoff_delay(63) <= Duration::from_secs(30));
            assert!(backoff_delay(64) <= Duration::from_secs(30));
        }
    }

    #[tokio::test]
    async fn reqwest_connect_refused_is_connection_lost() {
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let err = reqwest::Client::new()
            .get(format!("http://127.0.0.1:{port}/"))
            .send()
            .await
            .unwrap_err();
        assert_eq!(classify_reqwest(&err), ErrorKind::ConnectionLost);
    }

    #[tokio::test]
    async fn reqwest_timeout_is_timeout() {
        // Accepts connections but never answers.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let err = reqwest::Client::new()
            .get(format!("http://127.0.0.1:{port}/"))
            .timeout(Duration::from_millis(100))
            .send()
            .await
            .unwrap_err();
        assert_eq!(classify_reqwest(&err), ErrorKind::Timeout);
        drop(listener);
    }

    #[tokio::test]
    async fn reqwest_body_cut_short_is_connection_lost() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 1024];
            let _ = s.read(&mut buf).await;
            s.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 100\r\n\r\nshort")
                .await
                .unwrap();
        });
        let mut resp = reqwest::get(format!("http://127.0.0.1:{port}/"))
            .await
            .unwrap();
        let err = loop {
            match resp.chunk().await {
                Ok(Some(_)) => continue,
                Ok(None) => panic!("body ended cleanly"),
                Err(e) => break e,
            }
        };
        // reqwest wraps a failed body read as a decode error.
        assert!(err.is_decode(), "{err:?}");
        assert_eq!(classify_reqwest(&err), ErrorKind::ConnectionLost);
    }

    #[tokio::test]
    async fn reqwest_status_error_maps_by_status() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 1024];
            let _ = s.read(&mut buf).await;
            s.write_all(
                b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
            )
            .await
            .unwrap();
        });
        let err = reqwest::get(format!("http://127.0.0.1:{port}/"))
            .await
            .unwrap()
            .error_for_status()
            .unwrap_err();
        assert_eq!(classify_reqwest(&err), ErrorKind::LinkExpired);
    }
}
