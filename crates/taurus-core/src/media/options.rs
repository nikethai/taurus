//! What a video download can be told besides its quality: the browser
//! whose cookies it signs in with, subtitles, and the container. Checks of
//! those choices, and the yt-dlp arguments they become.

use std::ffi::OsString;

use crate::errors::{CoreError, ErrorKind};

/// The browsers yt-dlp reads cookies from.
const BROWSERS: [&str; 8] = [
    "chrome", "firefox", "safari", "edge", "brave", "chromium", "opera", "vivaldi",
];

/// Which subtitles to download with a video.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SubtitleChoice {
    /// Language codes such as `["en", "vi"]`, or `["all"]`.
    pub languages: Vec<String>,
    /// Also the captions the site generates.
    pub auto_generated: bool,
    /// Put them into the video file too (video only).
    pub embed: bool,
}

/// The file format a video is merged into.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Container {
    Mp4,
    Mkv,
    Webm,
}

impl Container {
    /// yt-dlp's `--merge-output-format` value.
    pub fn format(self) -> &'static str {
        match self {
            Container::Mp4 => "mp4",
            Container::Mkv => "mkv",
            Container::Webm => "webm",
        }
    }
}

fn refused(detail: &str) -> CoreError {
    CoreError {
        kind: ErrorKind::Unknown,
        detail: detail.to_owned(),
    }
}

/// Checks a browser for `--cookies-from-browser`: one of chrome, firefox,
/// safari, edge, brave, chromium, opera or vivaldi, optionally followed by
/// `:<profile>`. Anything else is `Unknown` ("unknown browser"), so that a
/// value is never passed to yt-dlp as an option or as its keyring and
/// container syntax.
pub fn validate_browser(spec: &str) -> Result<(), CoreError> {
    let (name, profile) = match spec.split_once(':') {
        Some((name, profile)) => (name, Some(profile)),
        None => (spec, None),
    };
    if !BROWSERS.contains(&name.to_ascii_lowercase().as_str()) {
        return Err(refused("unknown browser"));
    }
    match profile {
        Some(p)
            if p.trim().is_empty()
                || p.starts_with(':')
                || p.contains("::")
                || p.chars().any(char::is_control) =>
        {
            Err(refused("unknown browser profile"))
        }
        _ => Ok(()),
    }
}

/// Checks the languages of a subtitle choice: at least one, each a plain
/// code (letters, digits, `-` and `_`, not starting with `-`), so that
/// none is taken for an option or a yt-dlp pattern.
pub(crate) fn validate_subtitles(choice: &SubtitleChoice) -> Result<(), CoreError> {
    let plain = |l: &str| {
        !l.is_empty()
            && !l.starts_with('-')
            && l.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    };
    if choice.languages.is_empty() {
        return Err(refused("choose at least one subtitle language"));
    }
    if choice.languages.iter().all(|l| plain(l)) {
        Ok(())
    } else {
        Err(refused("unknown subtitle language"))
    }
}

/// The arguments for the cookies and subtitles: `--cookies-from-browser
/// <spec>`; `--write-subs`, `--write-auto-subs` (when `auto_generated`),
/// `--sub-langs <a,b|all,-live_chat>` (a stream's chat replay is not a
/// subtitle) and `--embed-subs` (when `embed`, for MP4 and MKV video; WebM
/// takes only WebVTT, which not every site has, so its subtitles stay
/// beside the video).
pub(crate) fn cookie_and_subtitle_args(
    cookies: Option<&str>,
    subtitles: Option<&SubtitleChoice>,
    audio: bool,
    container: Option<Container>,
) -> Vec<OsString> {
    let mut args = Vec::new();
    if let Some(browser) = cookies {
        args.push("--cookies-from-browser".into());
        args.push(browser.into());
    }
    if let Some(subs) = subtitles {
        args.push("--write-subs".into());
        if subs.auto_generated {
            args.push("--write-auto-subs".into());
        }
        let languages = if subs.languages.iter().any(|l| l == "all") {
            "all,-live_chat".to_owned()
        } else {
            subs.languages.join(",")
        };
        args.push("--sub-langs".into());
        args.push(languages.into());
        if subs.embed && !audio && container != Some(Container::Webm) {
            args.push("--embed-subs".into());
        }
    }
    args
}
