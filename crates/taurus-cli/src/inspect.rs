//! `inspect`: what a link offers (a video's title, qualities, subtitle and
//! caption languages; a file's name and size; a torrent's files), read the
//! way `add` reads it, without adding a job.

use std::process::ExitCode;

use clap::Args;
use taurus_core::media::{MediaInfo, QualityOption};
use taurus_core::{Engine, LinkInfo};

use crate::{Result, render};

#[derive(Args, Clone, Debug)]
pub struct InspectArgs {
    /// A video or audio link, a web link, a magnet link or a .torrent file or link
    #[arg(value_name = "URL")]
    pub url: String,
    /// Videos: read the link signed in with the cookies of a browser (chrome, firefox,
    /// safari, edge, brave, chromium, opera or vivaldi, optionally :PROFILE)
    #[arg(long, value_name = "BROWSER[:PROFILE]")]
    pub cookies_from: Option<String>,
}

/// Prints what the link offers; with `--json`, the engine's `LinkInfo` as
/// one JSON object (`{"Media": {...}}`, `{"File": {...}}` or
/// `{"Torrent": {...}}`).
pub async fn run(engine: &Engine, args: InspectArgs, json: bool) -> Result<ExitCode> {
    let info = engine
        .inspect_with_cookies(&args.url, args.cookies_from.as_deref())
        .await?;
    if json {
        println!("{}", serde_json::to_string(&info)?);
    } else {
        print!("{}", text(&info));
    }
    Ok(ExitCode::SUCCESS)
}

fn text(info: &LinkInfo) -> String {
    match info {
        LinkInfo::Media(media) => media_text(media),
        LinkInfo::File { name, size, .. } => {
            let size = size.map_or_else(|| "unknown".to_string(), render::bytes);
            format!("File: {name}\nSize: {size}\n")
        }
        LinkInfo::Torrent(torrent) => render::torrent_files(torrent),
    }
}

fn media_text(media: &MediaInfo) -> String {
    let mut out = format!("Title: {}\n", media.title);
    if let Some(playlist) = &media.playlist {
        out.push_str(&format!("Playlist: {} videos\n", playlist.count));
    }
    let qualities: Vec<String> = offered(media).into_iter().map(quality_text).collect();
    let languages = |list: &[String]| match list {
        [] => "none".to_string(),
        list => list.join(", "),
    };
    out.push_str(&format!("Qualities: {}\n", qualities.join(", ")));
    out.push_str(&format!(
        "Subtitles: {}\n",
        languages(&media.subtitle_languages)
    ));
    out.push_str(&format!(
        "Auto captions: {}\n",
        languages(&media.auto_caption_languages)
    ));
    out
}

/// Every quality `--quality` can pick for the video: `best`, then each
/// height (the usual ones and every other the video has), tallest first,
/// then the sound-only ones.
fn offered(media: &MediaInfo) -> Vec<&QualityOption> {
    let height = |q: &QualityOption| {
        q.id.strip_suffix('p')
            .and_then(|h| h.parse::<u64>().ok())
            .filter(|_| !q.audio_only)
    };
    let mut heights: Vec<&QualityOption> = Vec::new();
    for quality in media.qualities.iter().chain(&media.all_qualities) {
        if height(quality).is_some() && !heights.iter().any(|q| q.id == quality.id) {
            heights.push(quality);
        }
    }
    heights.sort_by_key(|q| std::cmp::Reverse(height(q)));
    let best = media.qualities.iter().filter(|q| q.id == "best");
    let audio = media.qualities.iter().filter(|q| q.audio_only);
    best.chain(heights).chain(audio).collect()
}

fn quality_text(quality: &QualityOption) -> String {
    match quality.est_size {
        Some(size) => format!("{} (~{})", quality.id, render::bytes(size)),
        None => quality.id.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use taurus_core::media::LiveStatus;

    fn option(id: &str, est_size: Option<u64>) -> QualityOption {
        QualityOption {
            id: id.to_string(),
            selector: String::new(),
            audio_only: id.starts_with("audio"),
            est_size,
        }
    }

    #[test]
    fn every_quality_and_language_is_listed() {
        let media = MediaInfo {
            url: String::new(),
            title: "A Fine Video".into(),
            uploader: None,
            duration_secs: None,
            thumbnail_url: None,
            extractor: "Youtube".into(),
            live: LiveStatus::NotLive,
            playlist: None,
            qualities: vec![
                option("best", None),
                option("1080p", Some(2048)),
                option("720p", None),
                option("audio-best", None),
                option("audio-small", None),
            ],
            subtitle_languages: vec!["en".into(), "vi".into()],
            auto_caption_languages: Vec::new(),
            all_qualities: vec![
                option("2160p", None),
                option("1080p", Some(2048)),
                option("720p", None),
                option("480p", None),
            ],
        };
        assert_eq!(
            media_text(&media),
            "Title: A Fine Video\n\
             Qualities: best, 2160p, 1080p (~2.0 KiB), 720p, 480p, audio-best, audio-small\n\
             Subtitles: en, vi\n\
             Auto captions: none\n"
        );
    }
}
