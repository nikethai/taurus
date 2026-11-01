//! Seeding policy, rate splitting and librqbit error mapping.

use std::time::Duration;

use crate::errors::ErrorKind;

/// When to stop sharing a finished torrent. Either condition stops it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SeedPolicy {
    /// `Some(0.0)` stops as soon as the download finishes; `None` never stops.
    pub stop_ratio: Option<f64>,
    pub stop_after: Option<Duration>,
}

impl SeedPolicy {
    /// `uploaded` and `selected_total` are bytes (ratio = uploaded / size of
    /// the selected files); `seeding_for` is the time since the download
    /// finished.
    pub fn should_stop(&self, uploaded: u64, selected_total: u64, seeding_for: Duration) -> bool {
        let ratio_reached = self.stop_ratio.is_some_and(|ratio| {
            // A zero or negative target is met by definition, even for an
            // empty selection; otherwise compare in floating point.
            ratio <= 0.0 || (selected_total > 0 && uploaded as f64 >= ratio * selected_total as f64)
        });
        let time_reached = self.stop_after.is_some_and(|limit| seeding_for >= limit);
        ratio_reached || time_reached
    }
}

/// Splits the global download limit between HTTP and torrents, as
/// `(http, torrent)`. The limit is halved only while both kinds are
/// downloading; otherwise each side keeps the full limit (so a job that
/// starts before the next recompute is still capped). `None` means unlimited.
pub fn split_rate(
    global: Option<u64>,
    http_active: bool,
    torrent_active: bool,
) -> (Option<u64>, Option<u64>) {
    match global {
        Some(g) if http_active && torrent_active => (Some(g / 2), Some(g - g / 2)),
        other => (other, other),
    }
}

/// The torrent side's download limit: the stricter of its share of the global
/// limit and the torrent-only limit.
pub fn torrent_rate(global_share: Option<u64>, torrent_only: Option<u64>) -> Option<u64> {
    match (global_share, torrent_only) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

/// Whether `message` (lowercase) says `os error <code>` for one of `codes`,
/// not merely a longer code with the same prefix (`os error 5` vs `50`).
fn os_error(message: &str, codes: &[&str]) -> bool {
    message.match_indices("os error ").any(|(i, needle)| {
        let digits: String = message[i + needle.len()..]
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        codes.contains(&digits.as_str())
    })
}

/// Maps a librqbit error message to a kind.
pub fn classify_torrent_error(message: &str) -> ErrorKind {
    let m = message.to_lowercase();
    let has = |needles: &[&str]| needles.iter().any(|n| m.contains(n));
    if has(&["no space left"]) || os_error(&m, &["28", "112"]) {
        ErrorKind::DiskFull
    } else if has(&["permission denied"]) || os_error(&m, &["13", "5"]) {
        ErrorKind::NoPermission
    } else if has(&[
        // librqbit-core's validate() messages for unsafe file names.
        "path traversal",
        "suspicious separator",
    ]) {
        ErrorKind::UnsafeTorrent
    } else if has(&[
        // A magnet's metadata could not be fetched from anyone.
        "no known way to resolve peers",
        "no way to discover torrent metainfo",
    ]) {
        ErrorKind::NoPeers
    } else {
        ErrorKind::Unknown
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NEVER: Duration = Duration::ZERO;

    fn ratio(r: f64) -> SeedPolicy {
        SeedPolicy {
            stop_ratio: Some(r),
            stop_after: None,
        }
    }

    #[test]
    fn ratio_stop() {
        let p = ratio(1.0);
        assert!(!p.should_stop(999, 1000, NEVER));
        assert!(p.should_stop(1000, 1000, NEVER));
        assert!(p.should_stop(5000, 1000, NEVER));
        assert!(!ratio(2.0).should_stop(1999, 1000, NEVER));
    }

    #[test]
    fn zero_ratio_stops_immediately() {
        assert!(ratio(0.0).should_stop(0, 1000, NEVER));
        assert!(ratio(0.0).should_stop(0, 0, NEVER));
    }

    #[test]
    fn empty_selection_never_reaches_a_positive_ratio() {
        assert!(!ratio(1.0).should_stop(0, 0, NEVER));
    }

    #[test]
    fn never_stop() {
        let p = SeedPolicy {
            stop_ratio: None,
            stop_after: None,
        };
        assert!(!p.should_stop(100_000, 1000, Duration::from_secs(10_000_000)));
    }

    #[test]
    fn time_stop() {
        let p = SeedPolicy {
            stop_ratio: None,
            stop_after: Some(Duration::from_secs(60)),
        };
        assert!(!p.should_stop(0, 1000, Duration::from_secs(59)));
        assert!(p.should_stop(0, 1000, Duration::from_secs(60)));
        // Whichever comes first.
        let both = SeedPolicy {
            stop_ratio: Some(1.0),
            stop_after: Some(Duration::from_secs(60)),
        };
        assert!(both.should_stop(1000, 1000, Duration::from_secs(1)));
        assert!(both.should_stop(0, 1000, Duration::from_secs(61)));
        assert!(!both.should_stop(10, 1000, Duration::from_secs(1)));
    }

    #[test]
    fn split_rate_cases() {
        assert_eq!(split_rate(Some(10), true, true), (Some(5), Some(5)));
        assert_eq!(split_rate(Some(11), true, true), (Some(5), Some(6)));
        // An idle side keeps the full limit.
        assert_eq!(split_rate(Some(10), false, true), (Some(10), Some(10)));
        assert_eq!(split_rate(Some(10), true, false), (Some(10), Some(10)));
        assert_eq!(split_rate(Some(10), false, false), (Some(10), Some(10)));
        assert_eq!(split_rate(None, true, true), (None, None));
    }

    #[test]
    fn torrent_rate_min() {
        assert_eq!(torrent_rate(Some(5), Some(3)), Some(3));
        assert_eq!(torrent_rate(Some(3), Some(5)), Some(3));
        assert_eq!(torrent_rate(Some(5), None), Some(5));
        assert_eq!(torrent_rate(None, Some(5)), Some(5));
        assert_eq!(torrent_rate(None, None), None);
    }

    #[test]
    fn error_mapping() {
        let k = classify_torrent_error;
        assert_eq!(
            k("write failed: No space left on device"),
            ErrorKind::DiskFull
        );
        assert_eq!(k("io error (os error 28)"), ErrorKind::DiskFull);
        assert_eq!(
            k("There is not enough space (os error 112)"),
            ErrorKind::DiskFull
        );
        assert_eq!(
            k("Permission denied (os error 13)"),
            ErrorKind::NoPermission
        );
        assert_eq!(k("Access is denied. (os error 5)"), ErrorKind::NoPermission);
        assert_eq!(
            k("path traversal detected, \"..\" in filename"),
            ErrorKind::UnsafeTorrent
        );
        assert_eq!(
            k("suspicious separator in filename"),
            ErrorKind::UnsafeTorrent
        );
        assert_eq!(
            k("no known way to resolve peers (no DHT, no trackers, no initial_peers)"),
            ErrorKind::NoPeers
        );
        assert_eq!(
            k("input address stream exhausted, no way to discover torrent metainfo"),
            ErrorKind::NoPeers
        );
        assert_eq!(k("connection reset"), ErrorKind::Unknown);
        assert_eq!(k("failed (os error 50)"), ErrorKind::Unknown);
    }
}
