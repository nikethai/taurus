//! Reading what a link holds: `yt-dlp --flat-playlist -J <url>`, and turning
//! its JSON into a [`MediaInfo`].

use std::time::Duration;

use serde_json::Value;

use crate::errors::{CoreError, ErrorKind};

use super::errors::{classify_stderr, error_line};
use super::process::run_captured;
use super::{LiveStatus, MediaInfo, PlaylistInfo, QualityOption, YtDlp, validate_browser};

const BEST: &str = "bv*+ba/b";
const AUDIO: &str = "ba/b";
/// Video heights that get an option of their own, highest first.
const CAPS: [u64; 2] = [1080, 720];
/// The tallest `<height>p` id taken for a quality (8K, with room to spare).
const MAX_HEIGHT: u64 = 8640;

/// Hosts whose links go to yt-dlp without first asking the web server
/// what they are (a page there is never a file).
const FAST_PATH_HOSTS: [&str; 14] = [
    "youtube.com",
    "youtu.be",
    "m.youtube.com",
    "music.youtube.com",
    "vimeo.com",
    "twitch.tv",
    "soundcloud.com",
    "x.com",
    "twitter.com",
    "tiktok.com",
    "instagram.com",
    "facebook.com",
    "dailymotion.com",
    "bilibili.com",
];

/// Whether `url` is an http(s) link to one of the well-known video hosts
/// (or a subdomain of one, such as `www.youtube.com`).
pub(crate) fn fast_path_host(url: &str) -> bool {
    let Ok(parsed) = url::Url::parse(url.trim()) else {
        return false;
    };
    if !matches!(parsed.scheme(), "http" | "https") {
        return false;
    }
    let Some(host) = parsed.host_str() else {
        return false;
    };
    let host = host.to_ascii_lowercase();
    FAST_PATH_HOSTS
        .iter()
        .any(|h| host == *h || host.strip_suffix(h).is_some_and(|p| p.ends_with('.')))
}

/// Runs `yt-dlp --flat-playlist -J <url>` (installing yt-dlp and Deno
/// first if they are missing) and reads what it prints. A link yt-dlp has
/// no extractor for is `SiteNotSupported`; its other failures are mapped by
/// [`classify_stderr`](super::errors); a run longer than `timeout` is killed
/// (with everything it started) and fails with `Timeout`. yt-dlp uses
/// `proxy`, and with `cookies` (a checked [`validate_browser`](super::validate_browser)
/// spec) the cookies of that browser, for links that need a sign-in.
pub async fn inspect(
    tools: &crate::tools::ToolManager,
    url: &str,
    proxy: &crate::network::ProxySetting,
    cookies: Option<&str>,
    timeout: Duration,
) -> Result<MediaInfo, CoreError> {
    let url = url.trim();
    if let Some(spec) = cookies {
        validate_browser(spec)?;
    }
    let yt_dlp = YtDlp::prepare(tools, proxy).await?;
    let mut command = yt_dlp.command();
    if single_video_link(url) {
        command.arg("--no-playlist");
    }
    if let Some(spec) = cookies {
        command.args(["--cookies-from-browser", spec]);
    }
    // `--` so that a link can never be taken for an option.
    command.args(["--flat-playlist", "-J", "--", url]);
    let out = run_captured(command, timeout).await?;
    if !out.success {
        return Err(CoreError {
            kind: classify_stderr(&out.stderr).0,
            detail: failure_detail(&out.stderr),
        });
    }
    let json: Value = serde_json::from_slice(&out.stdout)
        .map_err(|e| failed(format!("yt-dlp printed something that is not JSON: {e}")))?;
    let mut info = parse_info(&json)?;
    if info.url.is_empty() {
        info.url = url.to_owned();
    }
    Ok(info)
}

/// What to show for a failed run: its last `ERROR:` line without the
/// prefix, else its last line.
pub(crate) fn failure_detail(stderr: &str) -> String {
    error_line(stderr)
        .or_else(|| stderr.lines().map(str::trim).rfind(|l| !l.is_empty()))
        .unwrap_or("yt-dlp failed without a message")
        .to_owned()
}

/// Whether `url` is a YouTube link to one video that also names a playlist
/// (`watch?v=<id>&list=<id>` on youtube.com, m.youtube.com or
/// music.youtube.com; `youtu.be/<id>?list=<id>`). Such a link means the
/// video: inspect and download it with `--no-playlist`. A `/playlist?list=`
/// link is a playlist and is not matched.
pub fn single_video_link(url: &str) -> bool {
    let Ok(parsed) = url::Url::parse(url.trim()) else {
        return false;
    };
    if !matches!(parsed.scheme(), "http" | "https") {
        return false;
    }
    let Some(host) = parsed.host_str().map(str::to_ascii_lowercase) else {
        return false;
    };
    let has_param = |name: &str| {
        parsed
            .query_pairs()
            .any(|(k, v)| k == name && !v.trim().is_empty())
    };
    let on = |domain: &str| {
        host == domain || host.strip_suffix(domain).is_some_and(|p| p.ends_with('.'))
    };
    if on("youtube.com") {
        parsed.path().trim_end_matches('/') == "/watch" && has_param("v") && has_param("list")
    } else if on("youtu.be") {
        let id = parsed.path().trim_matches('/');
        !id.is_empty() && !id.contains('/') && has_param("list")
    } else {
        false
    }
}

fn failed(detail: String) -> CoreError {
    CoreError {
        kind: ErrorKind::ToolFailed,
        detail,
    }
}

/// Reads yt-dlp's `-J` output for a video or a playlist. A playlist
/// (`_type` `playlist` or `multi_video`) gets its title, a count of its
/// entries that are not `null`, and the options `best` and `audio-best`
/// without sizes; a video's options and sizes come from its `formats`. `url` is `webpage_url` (else
/// `original_url`; `inspect` fills in the link it was given if neither is
/// there). Fails with `ToolFailed` if there is no title (or id) to show.
pub fn parse_info(json: &Value) -> Result<MediaInfo, CoreError> {
    let text = |key: &str| {
        json.get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    let title = text("title")
        .or_else(|| text("fulltitle"))
        .or_else(|| text("id"))
        .ok_or_else(|| failed("yt-dlp's answer has no title".into()))?;
    let is_playlist = matches!(
        json.get("_type").and_then(Value::as_str),
        Some("playlist" | "multi_video")
    );
    let (playlist, qualities, all_qualities) = if is_playlist {
        // Flat playlists hold `null` for items that are unavailable: they
        // will not download, so they are not counted.
        let count = json
            .get("entries")
            .and_then(Value::as_array)
            .map_or(0, |entries| entries.iter().filter(|e| !e.is_null()).count());
        let playlist = PlaylistInfo {
            title: title.clone(),
            count: u32::try_from(count).unwrap_or(u32::MAX),
        };
        (Some(playlist), qualities(&[]), Vec::new())
    } else {
        let formats = json.get("formats").and_then(Value::as_array);
        let formats = formats.map_or(&[][..], Vec::as_slice);
        (None, qualities(formats), all_qualities(formats))
    };
    Ok(MediaInfo {
        url: text("webpage_url")
            .or_else(|| text("original_url"))
            .unwrap_or_default(),
        title,
        uploader: text("uploader")
            .or_else(|| text("channel"))
            .or_else(|| text("uploader_id")),
        duration_secs: json.get("duration").and_then(whole),
        thumbnail_url: text("thumbnail").or_else(|| last_thumbnail(json)),
        extractor: text("extractor_key")
            .or_else(|| text("extractor"))
            .unwrap_or_else(|| "generic".to_owned()),
        live: live_status(json),
        playlist,
        qualities,
        subtitle_languages: languages(json, "subtitles"),
        auto_caption_languages: languages(json, "automatic_captions"),
        all_qualities,
    })
}

/// The sorted language codes under `key` (`subtitles` or
/// `automatic_captions`); the chat replay of a stream is not a language.
fn languages(json: &Value, key: &str) -> Vec<String> {
    let mut out: Vec<String> = json
        .get(key)
        .and_then(Value::as_object)
        .map(|m| m.keys().filter(|k| *k != "live_chat").cloned().collect())
        .unwrap_or_default();
    out.sort();
    out
}

fn last_thumbnail(json: &Value) -> Option<String> {
    json.get("thumbnails")?
        .as_array()?
        .iter()
        .rev()
        .find_map(|t| t.get("url")?.as_str())
        .map(str::to_owned)
}

/// A JSON number as whole non-negative units (durations and sizes come as
/// integers or floats).
fn whole(v: &Value) -> Option<u64> {
    v.as_u64()
        .or_else(|| v.as_f64().filter(|f| *f >= 0.0).map(|f| f as u64))
}

fn live_status(json: &Value) -> LiveStatus {
    let flag = |key: &str| json.get(key).and_then(Value::as_bool) == Some(true);
    match json.get("live_status").and_then(Value::as_str) {
        Some("is_live") => LiveStatus::IsLive,
        Some("is_upcoming") => upcoming(json),
        Some("was_live" | "post_live") => LiveStatus::WasLive,
        Some(_) => LiveStatus::NotLive,
        None if flag("is_live") => LiveStatus::IsLive,
        None if flag("is_upcoming") => upcoming(json),
        None if flag("was_live") => LiveStatus::WasLive,
        None => LiveStatus::NotLive,
    }
}

fn upcoming(json: &Value) -> LiveStatus {
    LiveStatus::IsUpcoming {
        starts_at: json
            .get("release_timestamp")
            .and_then(|v| v.as_i64().or_else(|| v.as_f64().map(|f| f as i64))),
    }
}

/// One of a video's formats, as far as sizes and options care.
#[derive(Clone)]
struct Format {
    height: Option<u64>,
    size: Option<u64>,
    /// No audio in it: it needs an audio format to be merged with.
    video_only: bool,
}

/// What the `formats` of a video hold: its video formats, the sizes of
/// its audio-only formats, and whether there is any audio.
struct Scan {
    videos: Vec<Format>,
    audio_sizes: Vec<u64>,
    audio_any: bool,
}

fn scan(formats: &[Value]) -> Scan {
    let mut videos = Vec::new();
    let mut audio_sizes = Vec::new();
    let mut audio_any = false;
    for f in formats {
        let codec = |key: &str| f.get(key).and_then(Value::as_str);
        let (vcodec, acodec) = (codec("vcodec"), codec("acodec"));
        let height = f.get("height").and_then(whole);
        let size = f
            .get("filesize")
            .and_then(whole)
            .or_else(|| f.get("filesize_approx").and_then(whole));
        let storyboard = codec("ext") == Some("mhtml");
        let has_video = vcodec != Some("none") && (vcodec.is_some() || height.is_some());
        if storyboard {
            continue;
        }
        if has_video {
            let video_only = acodec == Some("none");
            audio_any |= !video_only;
            videos.push(Format {
                height,
                size,
                video_only,
            });
        } else if matches!(acodec, Some(a) if a != "none") {
            audio_any = true;
            audio_sizes.extend(size);
        }
    }
    Scan {
        videos,
        audio_sizes,
        audio_any,
    }
}

/// One option per distinct video height in `formats`, highest first, with
/// the id `<height>p`; the size is the largest format of that height (plus
/// the audio it lacks), as in [`qualities`].
pub fn all_qualities(formats: &[Value]) -> Vec<QualityOption> {
    let Scan {
        videos,
        audio_sizes,
        ..
    } = scan(formats);
    let audio_best = audio_sizes.iter().copied().max();
    let mut heights: Vec<u64> = videos
        .iter()
        .filter_map(|v| v.height)
        .filter(|h| *h > 0)
        .collect();
    heights.sort_unstable_by(|a, b| b.cmp(a));
    heights.dedup();
    heights
        .into_iter()
        .map(|h| {
            // The size of what the option picks: the formats of this height.
            let exact: Vec<Format> = videos
                .iter()
                .filter(|v| v.height == Some(h))
                .cloned()
                .collect();
            let mut quality = height_quality(h);
            quality.est_size = video_size(&exact, audio_best, h);
            quality
        })
        .collect()
}

/// The option for videos up to `height`.
fn height_quality(height: u64) -> QualityOption {
    let selector = format!("bv*[height<={height}]+ba/b[height<={height}]");
    option(&format!("{height}p"), &selector, false, None)
}

/// What the `formats` of a video allow, best first: `best`; `1080p` and
/// `720p` when the video reaches that height (and has a format at or below
/// it); `audio-best` and `audio-small` when there is audio. Without any
/// usable format (a direct link, a playlist), `best` and `audio-best`
/// without sizes. A video with no picture has audio options only.
///
/// `est_size` is the largest video format's `filesize` (or
/// `filesize_approx`) among those the option allows, plus the largest
/// audio-only format's when that video has no audio of its own; `None` when
/// no video size is known. `audio-best` is the largest audio-only format;
/// `audio-small` is re-encoded, so it has none.
pub fn qualities(formats: &[Value]) -> Vec<QualityOption> {
    let Scan {
        videos,
        audio_sizes,
        audio_any,
    } = scan(formats);
    let audio_best = audio_sizes.iter().copied().max();
    if videos.is_empty() && !audio_any {
        return vec![
            option("best", BEST, false, None),
            option("audio-best", AUDIO, true, None),
        ];
    }
    let mut out = Vec::new();
    if !videos.is_empty() {
        out.push(option(
            "best",
            BEST,
            false,
            video_size(&videos, audio_best, u64::MAX),
        ));
        let tallest = videos.iter().filter_map(|v| v.height).max();
        for cap in CAPS {
            let reached = tallest.is_some_and(|h| h >= cap);
            if reached && videos.iter().any(|v| v.height.is_some_and(|h| h <= cap)) {
                let mut quality = height_quality(cap);
                quality.est_size = video_size(&videos, audio_best, cap);
                out.push(quality);
            }
        }
    }
    if audio_any {
        out.push(option("audio-best", AUDIO, true, audio_best));
        out.push(option("audio-small", AUDIO, true, None));
    }
    out
}

/// The quality option `id` names, as [`qualities`] builds it, without a
/// size: for a link whose own list lacks it (a playlist lists only `best`
/// and `audio-best`). `None` for an id that is not one of the five.
pub(crate) fn standard_quality(id: &str) -> Option<QualityOption> {
    match id {
        "best" => Some(option(id, BEST, false, None)),
        "audio-best" | "audio-small" => Some(option(id, AUDIO, true, None)),
        _ => id
            .strip_suffix('p')
            .filter(|h| !h.is_empty() && h.bytes().all(|b| b.is_ascii_digit()))
            .and_then(|h| h.parse::<u64>().ok())
            .filter(|h| (1..=MAX_HEIGHT).contains(h))
            .map(height_quality),
    }
}

fn option(id: &str, selector: &str, audio_only: bool, est_size: Option<u64>) -> QualityOption {
    QualityOption {
        id: id.to_owned(),
        selector: selector.to_owned(),
        audio_only,
        est_size,
    }
}

/// The largest known size among `videos` no taller than `cap` (a format of
/// unknown height counts only when there is no cap), plus the best audio's
/// if that video has none of its own.
fn video_size(videos: &[Format], audio_best: Option<u64>, cap: u64) -> Option<u64> {
    let largest = videos
        .iter()
        .filter(|v| cap == u64::MAX || v.height.is_some_and(|h| h <= cap))
        .filter_map(|v| v.size.map(|s| (s, v.video_only)))
        .max_by_key(|(size, _)| *size)?;
    let (size, video_only) = largest;
    Some(size.saturating_add(if video_only {
        audio_best.unwrap_or(0)
    } else {
        0
    }))
}
