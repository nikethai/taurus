//! What yt-dlp's complaints mean: its stderr mapped to an [`ErrorKind`].

use crate::errors::ErrorKind;

/// The last line of `stderr` that starts with `ERROR:`, without the prefix.
pub(crate) fn error_line(stderr: &str) -> Option<&str> {
    stderr
        .lines()
        .map(str::trim)
        .rev()
        .find_map(|l| l.strip_prefix("ERROR:"))
        .map(str::trim)
}

/// The kind of failure yt-dlp's stderr describes, and whether it means the
/// extractor broke (the site changed and a newer yt-dlp may cope). Same as
/// [`classify_stderr_with`] for a quality chosen from the list.
pub(crate) fn classify_stderr(stderr: &str) -> (ErrorKind, bool) {
    classify_stderr_with(stderr, false)
}

/// Looks at the last `ERROR:` line only (no such line is `Unknown`), with
/// case and the typographic apostrophe ignored, and returns the first
/// matching row of this table:
///
/// | the line has | kind |
/// |---|---|
/// | `not available in your country` (also `has not made this video available in your country`), `geo restricted`, `geo-restricted` | `MediaRegionLocked` |
/// | `Private video`, `Video unavailable`, `This video has been removed`, `has been terminated` | `MediaUnavailable` |
/// | `Sign in to confirm your age`, `age-restricted`, `members-only`, `Join this channel`, `requires authentication`, `login required` | `NeedsSignIn` |
/// | `Sign in to confirm you're not a bot`, `confirm you are not a bot` | `BlockedByBotCheck` |
/// | `HTTP Error 429`, `Too Many Requests` | `RateLimited` |
/// | `Unsupported URL` | `SiteNotSupported` |
/// | `Unable to download`, `timed out`, `Connection reset`, `getaddrinfo`, `Temporary failure in name resolution`, `Network is unreachable` | `ConnectionLost` |
/// | `Signature extraction failed`, `nsig extraction failed`, `Unable to extract` | `Unknown`, extractor broke |
/// | `Requested format is not available`, unless `custom_quality` | `Unknown`, extractor broke |
/// | `Postprocessing:`, `ffmpeg exited with code` | `ToolFailed` |
/// | anything else | `Unknown` |
///
/// `custom_quality` is whether the person typed the format themselves:
/// then "Requested format is not available" is their mistake, not a
/// broken extractor, and it is a plain `Unknown`.
///
/// The region row comes first because yt-dlp words some regional blocks as
/// "Video unavailable. This video is not available in your country".
pub(crate) fn classify_stderr_with(stderr: &str, custom_quality: bool) -> (ErrorKind, bool) {
    let Some(line) = error_line(stderr) else {
        return (ErrorKind::Unknown, false);
    };
    let line = line.to_lowercase().replace('\u{2019}', "'");
    let has = |needles: &[&str]| needles.iter().any(|n| line.contains(n));
    let kind = if has(&[
        "not available in your country",
        "has not made this video available in your country",
        "geo restricted",
        "geo-restricted",
    ]) {
        ErrorKind::MediaRegionLocked
    } else if has(&[
        "private video",
        "video unavailable",
        "this video has been removed",
        "has been terminated",
    ]) {
        ErrorKind::MediaUnavailable
    } else if has(&[
        "sign in to confirm your age",
        "age-restricted",
        "members-only",
        "join this channel",
        "requires authentication",
        "login required",
    ]) {
        ErrorKind::NeedsSignIn
    } else if has(&[
        "sign in to confirm you're not a bot",
        "confirm you are not a bot",
    ]) {
        ErrorKind::BlockedByBotCheck
    } else if has(&["http error 429", "too many requests"]) {
        ErrorKind::RateLimited
    } else if has(&["unsupported url"]) {
        ErrorKind::SiteNotSupported
    } else if has(&[
        "unable to download",
        "timed out",
        "connection reset",
        "getaddrinfo",
        "temporary failure in name resolution",
        "network is unreachable",
    ]) {
        ErrorKind::ConnectionLost
    } else if has(&[
        "signature extraction failed",
        "nsig extraction failed",
        "unable to extract",
    ]) || (!custom_quality && has(&["requested format is not available"]))
    {
        return (ErrorKind::Unknown, true);
    } else if has(&["postprocessing:", "ffmpeg exited with code"]) {
        ErrorKind::ToolFailed
    } else {
        ErrorKind::Unknown
    };
    (kind, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BROKE: bool = true;

    #[test]
    fn every_mapping_row() {
        for (stderr, kind, broke) in [
            // MediaUnavailable
            (
                "ERROR: [youtube] abc: Private video. Sign in if you've been granted access to this video",
                ErrorKind::MediaUnavailable,
                false,
            ),
            (
                "ERROR: [youtube] abc: Video unavailable",
                ErrorKind::MediaUnavailable,
                false,
            ),
            (
                "ERROR: [youtube] abc: Video unavailable. This video has been removed by the uploader",
                ErrorKind::MediaUnavailable,
                false,
            ),
            (
                "ERROR: [youtube] abc: This video has been removed for violating YouTube's Terms of Service",
                ErrorKind::MediaUnavailable,
                false,
            ),
            (
                "ERROR: [youtube] abc: This video is no longer available because the YouTube account associated with this video has been terminated.",
                ErrorKind::MediaUnavailable,
                false,
            ),
            // MediaRegionLocked
            (
                "ERROR: [youtube] abc: The uploader has not made this video available in your country",
                ErrorKind::MediaRegionLocked,
                false,
            ),
            (
                "ERROR: [youtube] abc: This video is not available in your country",
                ErrorKind::MediaRegionLocked,
                false,
            ),
            (
                "ERROR: [youtube] abc: Video unavailable. This video is not available in your country",
                ErrorKind::MediaRegionLocked,
                false,
            ),
            (
                "ERROR: [generic] This video is geo restricted. You might want to use a VPN",
                ErrorKind::MediaRegionLocked,
                false,
            ),
            (
                "ERROR: [bbc] x: This content is geo-restricted to the UK",
                ErrorKind::MediaRegionLocked,
                false,
            ),
            // NeedsSignIn
            (
                "ERROR: [youtube] abc: Sign in to confirm your age. This video may be inappropriate for some users.",
                ErrorKind::NeedsSignIn,
                false,
            ),
            (
                "ERROR: [youtube] abc: This video is age-restricted and only available on YouTube",
                ErrorKind::NeedsSignIn,
                false,
            ),
            (
                "ERROR: [youtube] abc: Join this channel to get access to members-only content like this video",
                ErrorKind::NeedsSignIn,
                false,
            ),
            (
                "ERROR: [vimeo] 1: This video is available to members-only",
                ErrorKind::NeedsSignIn,
                false,
            ),
            (
                "ERROR: [twitter] 1: NSFW tweet requires authentication",
                ErrorKind::NeedsSignIn,
                false,
            ),
            (
                "ERROR: [instagram] x: Login required",
                ErrorKind::NeedsSignIn,
                false,
            ),
            // BlockedByBotCheck
            (
                "ERROR: [youtube] abc: Sign in to confirm you\u{2019}re not a bot. Use --cookies-from-browser",
                ErrorKind::BlockedByBotCheck,
                false,
            ),
            (
                "ERROR: [youtube] abc: Sign in to confirm you're not a bot",
                ErrorKind::BlockedByBotCheck,
                false,
            ),
            (
                "ERROR: [site] x: Please confirm you are not a bot",
                ErrorKind::BlockedByBotCheck,
                false,
            ),
            // RateLimited
            (
                "ERROR: [youtube] abc: Unable to download webpage: HTTP Error 429: Too Many Requests",
                ErrorKind::RateLimited,
                false,
            ),
            (
                "ERROR: [tiktok] x: Too Many Requests",
                ErrorKind::RateLimited,
                false,
            ),
            // SiteNotSupported
            (
                "ERROR: Unsupported URL: https://example.com/blog/post",
                ErrorKind::SiteNotSupported,
                false,
            ),
            // ConnectionLost
            (
                "ERROR: [youtube] abc: Unable to download webpage: <urlopen error [Errno -3] Temporary failure in name resolution>",
                ErrorKind::ConnectionLost,
                false,
            ),
            (
                "ERROR: [youtube] abc: Unable to download API page: The read operation timed out",
                ErrorKind::ConnectionLost,
                false,
            ),
            (
                "ERROR: unable to download video data: [Errno 104] Connection reset by peer",
                ErrorKind::ConnectionLost,
                false,
            ),
            (
                "ERROR: [generic] x: <urlopen error [Errno -2] getaddrinfo failed>",
                ErrorKind::ConnectionLost,
                false,
            ),
            (
                "ERROR: [generic] x: <urlopen error [Errno 101] Network is unreachable>",
                ErrorKind::ConnectionLost,
                false,
            ),
            // The extractor broke
            (
                "ERROR: [youtube] abc: Signature extraction failed: Some formats may be missing",
                ErrorKind::Unknown,
                BROKE,
            ),
            (
                "ERROR: [youtube] abc: nsig extraction failed: Some formats may be missing",
                ErrorKind::Unknown,
                BROKE,
            ),
            (
                "ERROR: [generic] Unable to extract title; please report this issue on https://github.com/yt-dlp/yt-dlp/issues",
                ErrorKind::Unknown,
                BROKE,
            ),
            (
                "ERROR: [youtube] abc: Requested format is not available. Use --list-formats for a list of available formats",
                ErrorKind::Unknown,
                BROKE,
            ),
            // ToolFailed
            (
                "ERROR: Postprocessing: ffprobe and ffmpeg not found. Please install or provide the path using --ffmpeg-location",
                ErrorKind::ToolFailed,
                false,
            ),
            (
                "ERROR: Postprocessing: Conversion failed!",
                ErrorKind::ToolFailed,
                false,
            ),
            (
                "ERROR: ffmpeg exited with code 1",
                ErrorKind::ToolFailed,
                false,
            ),
            // Unknown
            (
                "ERROR: something nobody has seen",
                ErrorKind::Unknown,
                false,
            ),
        ] {
            assert_eq!(classify_stderr(stderr), (kind, broke), "{stderr}");
        }
    }

    #[test]
    fn matching_ignores_case() {
        assert_eq!(
            classify_stderr("ERROR: PRIVATE VIDEO").0,
            ErrorKind::MediaUnavailable
        );
        assert_eq!(
            classify_stderr("ERROR: http error 429").0,
            ErrorKind::RateLimited
        );
    }

    #[test]
    fn custom_quality_is_not_a_broken_extractor() {
        let stderr = "ERROR: [youtube] abc: Requested format is not available. Use --list-formats";
        assert_eq!(
            classify_stderr_with(stderr, true),
            (ErrorKind::Unknown, false)
        );
        assert_eq!(
            classify_stderr_with(stderr, false),
            (ErrorKind::Unknown, BROKE)
        );
        // The other breakage signs count either way.
        let sig = "ERROR: Signature extraction failed";
        assert_eq!(classify_stderr_with(sig, true), (ErrorKind::Unknown, BROKE));
    }

    #[test]
    fn uses_last_error_line() {
        let stderr = "ERROR: [youtube] abc: Video unavailable\n\
                      WARNING: retrying\n\
                      ERROR: Unable to download webpage: HTTP Error 429: Too Many Requests\n\
                      [download] done\n";
        assert_eq!(classify_stderr(stderr), (ErrorKind::RateLimited, false));
        assert_eq!(
            error_line(stderr),
            Some("Unable to download webpage: HTTP Error 429: Too Many Requests")
        );
    }

    #[test]
    fn only_error_lines_count() {
        // The words appear, but not on an ERROR line.
        let stderr = "WARNING: Private video\nERROR: something else\nPrivate video\n";
        assert_eq!(classify_stderr(stderr), (ErrorKind::Unknown, false));
    }

    #[test]
    fn no_error_line_is_unknown() {
        for stderr in ["", "\n\n", "WARNING: Private video", "Private video"] {
            assert_eq!(
                classify_stderr(stderr),
                (ErrorKind::Unknown, false),
                "{stderr:?}"
            );
            assert_eq!(error_line(stderr), None, "{stderr:?}");
        }
    }
}
