//! Video and audio links for `add`: the flags, the choice they make from what
//! yt-dlp reports for a link, and the plain-words error for a large playlist.

use clap::{Args, ValueEnum};
use taurus_core::media::{
    Container, ConvertPreset, MediaInfo, SubtitleChoice, standard_quality, validate_browser,
};
use taurus_core::{AddRequest, CoreError, Engine, ErrorKind, JobId, LinkInfo, MediaChoice};

/// The flags that choose what to save of a video.
#[derive(Args, Clone, Debug, Default)]
pub struct VideoFlags {
    /// Video links: save only the sound (best audio; with --quality small, a small MP3)
    #[arg(long)]
    pub audio: bool,
    /// Video links: best, audio (as --audio), small (with --audio: a small MP3), or a
    /// height such as 1080p, 720p or 480p (taurus inspect lists a video's); one that
    /// the video lacks falls back to best [default: best]
    #[arg(long, value_name = "QUALITY", value_parser = parse_quality)]
    pub quality: Option<Quality>,
    /// Video links: convert the result to an MP3 or a smaller MP4
    #[arg(long, value_enum, value_name = "FORMAT")]
    pub convert: Option<Convert>,
    /// Video links: also download a playlist of more than 50 videos
    #[arg(long)]
    pub yes: bool,
    /// Video links: sign in with the cookies of a browser: chrome, firefox, safari, edge, brave,
    /// chromium, opera or vivaldi, optionally followed by :PROFILE
    #[arg(long, value_name = "BROWSER[:PROFILE]")]
    pub cookies_from: Option<String>,
    /// Video links: download these subtitle languages, e.g. en,vi, or all
    #[arg(long, value_name = "LANGS", value_delimiter = ',')]
    pub subs: Vec<String>,
    /// Video links: with --subs, also the captions the site generates
    #[arg(long)]
    pub auto_subs: bool,
    /// Video links: with --subs, also put them into the video file (not for WebM or sound)
    #[arg(long)]
    pub embed_subs: bool,
    /// Video links: the file format of the video: mp4, mkv or webm [default: mp4]
    #[arg(long, value_enum, value_name = "FORMAT")]
    pub container: Option<ContainerFlag>,
}

/// What `--quality` takes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Quality {
    Best,
    /// The best sound only, as `--audio`.
    Audio,
    /// With `--audio`: a small MP3.
    Small,
    /// Videos up to this height: `<N>p`, as the engine names it.
    Height(String),
}

const QUALITY_VALUES: &str = "best, audio, small, or a height such as 1080p, 720p or 480p";

/// `best`, `audio`, `small` or `<N>p`, ignoring case; a height is checked
/// by the engine's own reading of quality names.
fn parse_quality(value: &str) -> Result<Quality, String> {
    let value = value.trim().to_ascii_lowercase();
    match value.as_str() {
        "best" => Ok(Quality::Best),
        "audio" => Ok(Quality::Audio),
        "small" => Ok(Quality::Small),
        _ if value.ends_with('p') && standard_quality(&value).is_some() => {
            Ok(Quality::Height(value))
        }
        _ => Err(format!("expected {QUALITY_VALUES}")),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum ContainerFlag {
    Mp4,
    Mkv,
    Webm,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Convert {
    Mp3,
    SmallerMp4,
}

impl VideoFlags {
    /// Whether a flag that shapes a video download was given.
    fn chooses(&self) -> bool {
        self.audio
            || self.quality.is_some()
            || self.convert.is_some()
            || self.cookies_from.is_some()
            || !self.subs.is_empty()
            || self.container.is_some()
    }

    /// A combination that means nothing, or a value no video takes, in
    /// plain words.
    pub fn conflict(&self) -> Option<String> {
        let say = |text: &str| Some(text.to_string());
        match (self.audio, &self.quality) {
            (false, Some(Quality::Small)) => return say("--quality small needs --audio"),
            (true, Some(Quality::Height(height))) => {
                return Some(format!(
                    "--audio cannot be combined with --quality {height}"
                ));
            }
            _ => {}
        }
        if self.subs.is_empty() && (self.auto_subs || self.embed_subs) {
            return say("--auto-subs and --embed-subs need --subs");
        }
        if self.convert == Some(Convert::SmallerMp4)
            && matches!(
                self.container,
                Some(ContainerFlag::Mkv | ContainerFlag::Webm)
            )
        {
            return say(
                "--convert smaller-mp4 makes an MP4: it cannot be combined with --container mkv or webm",
            );
        }
        if let Some(browser) = &self.cookies_from {
            return validate_browser(browser.trim())
                .err()
                .map(|e| format!("--cookies-from: {}", e.detail));
        }
        None
    }

    /// The browser to sign in with, if one was named.
    fn browser(&self) -> Option<&str> {
        self.cookies_from.as_deref().map(str::trim)
    }

    /// The choice for `info`, and a note when the quality asked for is not
    /// offered (best is used instead).
    fn choice(&self, info: &MediaInfo) -> (MediaChoice, Option<String>) {
        let convert = self.convert.map(|c| match c {
            Convert::Mp3 => ConvertPreset::Mp3,
            Convert::SmallerMp4 => ConvertPreset::SmallerMp4,
        });
        let (quality, note) = if self.audio || self.quality == Some(Quality::Audio) {
            let id = if self.quality == Some(Quality::Small) {
                "audio-small"
            } else {
                "audio-best"
            };
            (id, None)
        } else {
            let id = match &self.quality {
                Some(Quality::Height(height)) => height.as_str(),
                _ => "best",
            };
            // Every height the video has, not only the usual ones.
            let offered = info
                .qualities
                .iter()
                .chain(&info.all_qualities)
                .any(|q| q.id == id);
            if id != "best" && !offered {
                (
                    "best",
                    Some(format!(
                        "note: {id} is not available; using the best quality"
                    )),
                )
            } else {
                (id, None)
            }
        };
        let subtitles = (!self.subs.is_empty()).then(|| SubtitleChoice {
            languages: self.subs.iter().map(|l| l.trim().to_string()).collect(),
            auto_generated: self.auto_subs,
            embed: self.embed_subs,
        });
        let container = self.container.map(|c| match c {
            ContainerFlag::Mp4 => Container::Mp4,
            ContainerFlag::Mkv => Container::Mkv,
            ContainerFlag::Webm => Container::Webm,
        });
        let choice = MediaChoice {
            quality: quality.to_string(),
            convert,
            cookies_from_browser: self.browser().map(String::from),
            subtitles,
            container,
        };
        (choice, note)
    }
}

/// Adds one link. A link that yt-dlp reads gets `media` set, so the engine
/// downloads it as video; any other link is added as before.
pub async fn add(
    engine: &Engine,
    mut req: AddRequest,
    flags: &VideoFlags,
    torrent: bool,
) -> Result<JobId, CoreError> {
    req.confirm_large_playlist = flags.yes;
    if torrent {
        return engine.add(req).await;
    }
    // With a video flag, what the link offers decides the choice.
    if flags.chooses() {
        match engine
            .inspect_with_cookies(&req.url, flags.browser())
            .await?
        {
            LinkInfo::Media(info) => {
                let (choice, note) = flags.choice(&info);
                if let Some(note) = note {
                    eprintln!("{note}");
                }
                req.media = Some(choice);
                // The engine need not read the link again.
                req.media_info = Some(info);
            }
            _ => eprintln!(
                "note: {}: not a video link; ignoring the video options (--audio, --quality, --convert, --cookies-from, --subs, --container)",
                req.url
            ),
        }
        return engine.add(req).await;
    }
    // A video site is added as video as it is; a web page that is not one
    // is tried with yt-dlp before giving up.
    match engine.add(req.clone()).await {
        Err(e) if e.kind == ErrorKind::SiteNotSupported => match engine.inspect(&req.url).await {
            Ok(LinkInfo::Media(info)) => {
                req.media = Some(flags.choice(&info).0);
                req.media_info = Some(info);
                engine.add(req).await
            }
            _ => Err(e),
        },
        result => result,
    }
}

/// The error as shown: a large playlist says how to go on.
pub fn error_text(e: &CoreError) -> String {
    match (e.kind, e.detail.strip_prefix("playlist:")) {
        (ErrorKind::NeedsConfirmation, Some(count)) => format!(
            "This playlist has {count} videos. Run again with --yes to download all of them."
        ),
        _ => e.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use taurus_core::media::{LiveStatus, QualityOption};

    fn option(id: &str) -> QualityOption {
        QualityOption {
            id: id.to_string(),
            selector: String::new(),
            audio_only: id.starts_with("audio"),
            est_size: None,
        }
    }

    fn info(ids: &[&str]) -> MediaInfo {
        MediaInfo {
            url: String::new(),
            title: "T".into(),
            uploader: None,
            duration_secs: None,
            thumbnail_url: None,
            extractor: "Youtube".into(),
            live: LiveStatus::NotLive,
            playlist: None,
            qualities: ids.iter().map(|id| option(id)).collect(),
            subtitle_languages: Vec::new(),
            auto_caption_languages: Vec::new(),
            all_qualities: Vec::new(),
        }
    }

    fn height(id: &str) -> Quality {
        Quality::Height(id.to_string())
    }

    fn flags(audio: bool, quality: Option<Quality>, convert: Option<Convert>) -> VideoFlags {
        VideoFlags {
            audio,
            quality,
            convert,
            ..VideoFlags::default()
        }
    }

    #[test]
    fn flags_make_the_choice() {
        use ConvertPreset::{Mp3, SmallerMp4};
        let video = info(&["best", "1080p", "720p", "audio-best", "audio-small"]);
        let pick = |f: VideoFlags, i: &MediaInfo| {
            let (c, note) = f.choice(i);
            (c.quality, c.convert, note.is_some())
        };
        let want = |q: &str, c: Option<ConvertPreset>, note: bool| (q.to_string(), c, note);
        assert_eq!(
            pick(flags(false, None, None), &video),
            want("best", None, false)
        );
        assert_eq!(
            pick(flags(false, Some(height("720p")), None), &video),
            want("720p", None, false)
        );
        assert_eq!(
            pick(flags(true, None, None), &video),
            want("audio-best", None, false)
        );
        // R11: small audio sends the quality and no conversion of its own.
        assert_eq!(
            pick(flags(true, Some(Quality::Small), None), &video),
            want("audio-small", None, false)
        );
        assert_eq!(
            pick(
                flags(true, Some(Quality::Small), Some(Convert::Mp3)),
                &video
            ),
            want("audio-small", Some(Mp3), false)
        );
        assert_eq!(
            pick(flags(false, None, Some(Convert::SmallerMp4)), &video),
            want("best", Some(SmallerMp4), false)
        );
        // A playlist offers only best and audio-best: 1080p falls back, with a note.
        let playlist = info(&["best", "audio-best"]);
        assert_eq!(
            pick(flags(false, Some(height("1080p")), None), &playlist),
            want("best", None, true)
        );
        let (_, note) = flags(false, Some(height("1080p")), None).choice(&playlist);
        assert_eq!(
            note.unwrap(),
            "note: 1080p is not available; using the best quality"
        );
        // A height only the full list has is chosen as it is, without a note.
        let mut tall = info(&["best", "1080p", "720p", "audio-best"]);
        tall.all_qualities = ["2160p", "1080p", "720p", "480p"].map(option).to_vec();
        assert_eq!(
            pick(flags(false, Some(height("480p")), None), &tall),
            want("480p", None, false)
        );
        assert_eq!(
            pick(flags(false, Some(height("360p")), None), &tall),
            want("best", None, true)
        );
        // --quality audio is --audio.
        assert_eq!(
            pick(flags(false, Some(Quality::Audio), None), &video),
            want("audio-best", None, false)
        );
    }

    #[test]
    fn quality_values() {
        assert_eq!(parse_quality("best"), Ok(Quality::Best));
        assert_eq!(parse_quality(" Audio "), Ok(Quality::Audio));
        assert_eq!(parse_quality("small"), Ok(Quality::Small));
        assert_eq!(parse_quality("480p"), Ok(height("480p")));
        assert_eq!(parse_quality("1080P"), Ok(height("1080p")));
        for bad in ["", "720", "p", "0p", "-1p", "99999p", "audio-best", "hd"] {
            assert!(parse_quality(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn meaningless_combinations() {
        assert!(
            flags(false, Some(Quality::Small), None)
                .conflict()
                .is_some()
        );
        assert_eq!(
            flags(true, Some(height("480p")), None).conflict().unwrap(),
            "--audio cannot be combined with --quality 480p"
        );
        assert!(flags(true, Some(Quality::Small), None).conflict().is_none());
        assert!(flags(true, Some(Quality::Best), None).conflict().is_none());
        assert!(flags(false, Some(Quality::Best), None).conflict().is_none());
    }

    #[test]
    fn large_playlist_text() {
        let e = CoreError {
            kind: ErrorKind::NeedsConfirmation,
            detail: "playlist:51".into(),
        };
        assert_eq!(
            error_text(&e),
            "This playlist has 51 videos. Run again with --yes to download all of them."
        );
    }
}
