//! Video and audio links for `add`: the flags, the choice they make from what
//! yt-dlp reports for a link, and the plain-words error for a large playlist.

use clap::{Args, ValueEnum};
use taurus_core::media::{ConvertPreset, MediaInfo};
use taurus_core::{AddRequest, CoreError, Engine, ErrorKind, JobId, LinkInfo, MediaChoice};

/// The flags that choose what to save of a video.
#[derive(Args, Clone, Debug, Default)]
pub struct VideoFlags {
    /// Video links: save only the sound (best audio; with --quality small, a small MP3)
    #[arg(long)]
    pub audio: bool,
    /// Video links: best, 1080p or 720p (small, with --audio, makes a small MP3);
    /// one that the video lacks falls back to best [default: best]
    #[arg(long, value_enum, value_name = "QUALITY")]
    pub quality: Option<Quality>,
    /// Video links: convert the result to an MP3 or a smaller MP4
    #[arg(long, value_enum, value_name = "FORMAT")]
    pub convert: Option<Convert>,
    /// Video links: also download a playlist of more than 50 videos
    #[arg(long)]
    pub yes: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Quality {
    Best,
    #[value(name = "1080p")]
    P1080,
    #[value(name = "720p")]
    P720,
    Small,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Convert {
    Mp3,
    SmallerMp4,
}

impl VideoFlags {
    /// Whether a flag that shapes a video download was given.
    fn chooses(&self) -> bool {
        self.audio || self.quality.is_some() || self.convert.is_some()
    }

    /// A combination that means nothing, in plain words.
    pub fn conflict(&self) -> Option<&'static str> {
        match (self.audio, self.quality) {
            (false, Some(Quality::Small)) => Some("--quality small needs --audio"),
            (true, Some(Quality::P1080 | Quality::P720)) => {
                Some("--audio cannot be combined with --quality 1080p or 720p")
            }
            _ => None,
        }
    }

    /// The choice for `info`, and a note when the quality asked for is not
    /// offered (best is used instead).
    fn choice(&self, info: &MediaInfo) -> (MediaChoice, Option<String>) {
        let convert = self.convert.map(|c| match c {
            Convert::Mp3 => ConvertPreset::Mp3,
            Convert::SmallerMp4 => ConvertPreset::SmallerMp4,
        });
        let (quality, note) = if self.audio {
            let id = if self.quality == Some(Quality::Small) {
                "audio-small"
            } else {
                "audio-best"
            };
            (id, None)
        } else {
            let id = match self.quality {
                Some(Quality::P1080) => "1080p",
                Some(Quality::P720) => "720p",
                _ => "best",
            };
            if id != "best" && !info.qualities.iter().any(|q| q.id == id) {
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
        let choice = MediaChoice {
            quality: quality.to_string(),
            convert,
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
        match engine.inspect(&req.url).await? {
            LinkInfo::Media(info) => {
                let (choice, note) = flags.choice(&info);
                if let Some(note) = note {
                    eprintln!("{note}");
                }
                req.media = Some(choice);
            }
            _ => eprintln!(
                "note: {}: not a video link; ignoring --audio, --quality and --convert",
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
            qualities: ids
                .iter()
                .map(|id| QualityOption {
                    id: id.to_string(),
                    selector: String::new(),
                    audio_only: id.starts_with("audio"),
                    est_size: None,
                })
                .collect(),
        }
    }

    fn flags(audio: bool, quality: Option<Quality>, convert: Option<Convert>) -> VideoFlags {
        VideoFlags {
            audio,
            quality,
            convert,
            yes: false,
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
            pick(flags(false, Some(Quality::P720), None), &video),
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
            pick(flags(false, Some(Quality::P1080), None), &playlist),
            want("best", None, true)
        );
        let (_, note) = flags(false, Some(Quality::P1080), None).choice(&playlist);
        assert_eq!(
            note.unwrap(),
            "note: 1080p is not available; using the best quality"
        );
    }

    #[test]
    fn meaningless_combinations() {
        assert!(
            flags(false, Some(Quality::Small), None)
                .conflict()
                .is_some()
        );
        assert!(flags(true, Some(Quality::P720), None).conflict().is_some());
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
