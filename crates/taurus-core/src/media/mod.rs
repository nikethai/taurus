//! Video and audio links, handled by yt-dlp: what a link holds
//! ([`inspect`]) and the one way every yt-dlp run is started.
//!
//! The tools come from the `ToolManager` (yt-dlp itself, Deno for the
//! JavaScript some extractors need, and ffmpeg for merging and converting).

mod errors;
mod inspect;
mod options;
mod partials;
mod process;
pub(crate) mod remux;
pub(crate) mod run;
#[cfg(test)]
mod tests;
pub(crate) mod thumbnail;
mod tracker;

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use tokio::process::Command;

use crate::errors::CoreError;
use crate::network::{ProxySetting, proxy_env, ytdlp_proxy_args};
use crate::tools::{ToolKind, ToolManager};

pub(crate) use errors::{classify_stderr_with, live_from_start_refused, only_thumbnail_refused};
pub use inspect::{all_qualities, inspect, parse_info, qualities, single_video_link};
pub(crate) use inspect::{failure_detail, fast_path_host, standard_quality};
pub use options::{Container, SubtitleChoice, validate_browser};
pub(crate) use options::{cookie_and_subtitle_args, validate_subtitles};
pub(crate) use partials::own_partial_files;
pub use run::MediaPhase;

/// One way to download a video: yt-dlp's format selector, plus the
/// audio options [`QualityOption::extra_args`] adds.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct QualityOption {
    /// `best`, `1080p`, `720p`, `audio-best` or `audio-small`.
    pub id: String,
    /// The value of yt-dlp's `-f`.
    pub selector: String,
    pub audio_only: bool,
    /// The download's estimated size in bytes, if the formats say.
    pub est_size: Option<u64>,
}

impl QualityOption {
    /// The arguments besides `-f <selector>` that this option needs:
    /// `-x` for audio, plus `--audio-format mp3 --audio-quality 5` for
    /// `audio-small`; none for video.
    pub fn extra_args(&self) -> &'static [&'static str] {
        extra_args_for(&self.id)
    }
}

/// [`QualityOption::extra_args`] for the option with this id.
pub(crate) fn extra_args_for(id: &str) -> &'static [&'static str] {
    match id {
        "audio-best" => &["-x"],
        "audio-small" => &["-x", "--audio-format", "mp3", "--audio-quality", "5"],
        _ => &[],
    }
}

/// What a media job downloads and how, stored with the job. Rows stored
/// before a field existed read it as unset.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct MediaOptions {
    /// The chosen [`QualityOption::id`].
    pub quality: String,
    /// yt-dlp's `-f` for it.
    pub selector: String,
    pub audio_only: bool,
    pub convert: Option<ConvertPreset>,
    /// The link is a playlist: every entry is downloaded, into a folder
    /// named after the playlist.
    pub playlist: bool,
    /// The link was live or upcoming when inspected: it is recorded from
    /// its start, waiting for an upcoming stream to begin.
    pub live_from_start: bool,
    /// The files its runs wrote that are not finished, as yt-dlp reported
    /// them (`tmpfilename`, and the `filename` it becomes), so that removing
    /// the job deletes its own partial files and no one else's, even after
    /// a restart. A file reported finished leaves the list.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub partial_files: Vec<PathBuf>,
    /// The browser (`firefox`, or `chrome:Profile 1`) whose cookies yt-dlp
    /// signs in with; only its name goes to yt-dlp, which reads the cookies
    /// itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cookies_from_browser: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subtitles: Option<SubtitleChoice>,
    /// A video's container; `None` is MP4. Audio ignores it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container: Option<Container>,
}

/// A conversion after the download.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ConvertPreset {
    /// The audio as an MP3 (VBR quality 2).
    Mp3,
    /// The video re-encoded to a smaller MP4 (CRF 28).
    SmallerMp4,
}

impl ConvertPreset {
    /// The yt-dlp arguments for the conversion.
    pub fn args(self) -> &'static [&'static str] {
        match self {
            ConvertPreset::Mp3 => &["-x", "--audio-format", "mp3", "--audio-quality", "2"],
            ConvertPreset::SmallerMp4 => &[
                "--recode-video",
                "mp4",
                "--postprocessor-args",
                "VideoConvertor:-crf 28 -preset veryfast",
            ],
        }
    }
}

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub enum LiveStatus {
    NotLive,
    IsLive,
    /// A premiere or stream that has not begun; `starts_at` is unix seconds.
    IsUpcoming {
        starts_at: Option<i64>,
    },
    /// A stream that has ended (it may still be processing).
    WasLive,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct PlaylistInfo {
    pub title: String,
    pub count: u32,
}

/// What yt-dlp says a link holds.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct MediaInfo {
    pub url: String,
    pub title: String,
    pub uploader: Option<String>,
    pub duration_secs: Option<u64>,
    pub thumbnail_url: Option<String>,
    /// yt-dlp's extractor key, such as `Youtube`.
    pub extractor: String,
    pub live: LiveStatus,
    /// Set when the link is a playlist; then `title` is the playlist's.
    pub playlist: Option<PlaylistInfo>,
    pub qualities: Vec<QualityOption>,
    /// Languages of the subtitles the uploader made, sorted.
    pub subtitle_languages: Vec<String>,
    /// Languages of the captions the site generates, sorted.
    pub auto_caption_languages: Vec<String>,
    /// One option per distinct video height, highest first, with the ids
    /// `<height>p`; empty for a playlist or a video without a picture.
    pub all_qualities: Vec<QualityOption>,
}

/// The flags every yt-dlp run gets: Deno as the JavaScript runtime, the
/// folder holding ffmpeg and ffprobe, Windows-safe file names, plain,
/// line-by-line output without warnings or colors, and the proxy setting
/// ([`ytdlp_proxy_args`]).
pub fn global_args(deno: &Path, ffmpeg_dir: &Path, proxy: &ProxySetting) -> Vec<OsString> {
    let mut js_runtime = OsString::from("deno:");
    js_runtime.push(deno);
    let mut args: Vec<OsString> = vec![
        "--js-runtimes".into(),
        js_runtime,
        "--ffmpeg-location".into(),
        ffmpeg_dir.into(),
        "--windows-filenames".into(),
        "--no-warnings".into(),
        "--newline".into(),
        "--no-colors".into(),
    ];
    args.extend(ytdlp_proxy_args(proxy).into_iter().map(OsString::from));
    args
}

/// The installed yt-dlp, the global flags for it and the proxy variables
/// of its environment ([`proxy_env`]).
#[derive(Clone, Debug)]
pub(crate) struct YtDlp {
    path: PathBuf,
    global: Vec<OsString>,
    env: Vec<(&'static str, Option<String>)>,
}

impl YtDlp {
    /// Installs yt-dlp and Deno if they are missing (without reporting
    /// progress; `Engine` does that first), and finds ffmpeg's folder
    /// (which holds nothing until ffmpeg is installed). Its runs use `proxy`.
    pub(crate) async fn prepare(
        tools: &ToolManager,
        proxy: &ProxySetting,
    ) -> Result<YtDlp, CoreError> {
        let none = |_| {};
        let (yt_dlp, deno) = tokio::join!(
            tools.ensure(ToolKind::YtDlp, &none),
            tools.ensure(ToolKind::Deno, &none),
        );
        let (path, deno) = (yt_dlp?, deno?);
        let ffmpeg_dir = tools
            .status()
            .into_iter()
            .find(|s| s.kind == ToolKind::Ffmpeg)
            .and_then(|s| s.path)
            .and_then(|p| p.parent().map(Path::to_path_buf))
            .unwrap_or_else(|| tools.tool_dir(ToolKind::Ffmpeg));
        let global = global_args(&deno, &ffmpeg_dir, proxy);
        Ok(YtDlp {
            path,
            global,
            env: proxy_env(proxy),
        })
    }

    /// The yt-dlp binary this runs.
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// `yt-dlp <global flags>`, with stdin closed and the proxy variables
    /// of its environment set or removed. The caller adds the
    /// arguments and the other stdio, and starts it with
    /// `tools::tree::TreeChild::spawn`, so that stopping it stops
    /// everything it starts.
    pub(crate) fn command(&self) -> Command {
        let mut command = Command::new(&self.path);
        command
            .args(&self.global)
            .stdin(std::process::Stdio::null());
        for (name, value) in &self.env {
            match value {
                Some(value) => command.env(name, value),
                None => command.env_remove(name),
            };
        }
        command
    }
}
