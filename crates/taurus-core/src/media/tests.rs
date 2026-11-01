use serde_json::{Value, json};

use super::*;
use crate::errors::ErrorKind;

fn fixture(name: &str) -> Value {
    let path = format!("{}/tests/fixtures/ytdlp/{name}", env!("CARGO_MANIFEST_DIR"));
    let text = std::fs::read_to_string(&path).unwrap();
    serde_json::from_str(&text).unwrap()
}

fn ids(options: &[QualityOption]) -> Vec<&str> {
    options.iter().map(|o| o.id.as_str()).collect()
}

fn option<'a>(options: &'a [QualityOption], id: &str) -> &'a QualityOption {
    options.iter().find(|o| o.id == id).unwrap()
}

#[test]
fn parse_single_video() {
    let info = parse_info(&fixture("single_video.json")).unwrap();
    assert_eq!(info.url, "https://www.youtube.com/watch?v=abc123");
    assert_eq!(info.title, "A Fine Video");
    assert_eq!(info.uploader.as_deref(), Some("Some Channel"));
    assert_eq!(info.duration_secs, Some(212));
    assert_eq!(
        info.thumbnail_url.as_deref(),
        Some("https://i.example.com/abc123/maxres.jpg")
    );
    assert_eq!(info.extractor, "Youtube");
    assert_eq!(info.live, LiveStatus::NotLive);
    assert_eq!(info.playlist, None);
    assert_eq!(
        ids(&info.qualities),
        ["best", "1080p", "720p", "audio-best", "audio-small"]
    );
}

#[test]
fn parse_playlist() {
    let info = parse_info(&fixture("playlist.json")).unwrap();
    assert_eq!(info.title, "Road Trip Songs");
    assert_eq!(
        info.playlist,
        Some(PlaylistInfo {
            title: "Road Trip Songs".into(),
            count: 3
        })
    );
    assert_eq!(info.extractor, "YoutubeTab");
    assert_eq!(info.url, "https://www.youtube.com/playlist?list=PL123");
    assert_eq!(info.duration_secs, None);
    assert_eq!(
        info.qualities,
        [
            QualityOption {
                id: "best".into(),
                selector: "bv*+ba/b".into(),
                audio_only: false,
                est_size: None
            },
            QualityOption {
                id: "audio-best".into(),
                selector: "ba/b".into(),
                audio_only: true,
                est_size: None
            },
        ]
    );
}

#[test]
fn parse_live_and_upcoming() {
    assert_eq!(
        parse_info(&fixture("live.json")).unwrap().live,
        LiveStatus::IsLive
    );
    assert_eq!(
        parse_info(&fixture("upcoming.json")).unwrap().live,
        LiveStatus::IsUpcoming {
            starts_at: Some(1_793_600_000)
        }
    );
    assert_eq!(
        parse_info(&fixture("was_live.json")).unwrap().live,
        LiveStatus::WasLive
    );
    let mut post = fixture("was_live.json");
    post["live_status"] = json!("post_live");
    assert_eq!(parse_info(&post).unwrap().live, LiveStatus::WasLive);
    let mut soon = fixture("upcoming.json");
    soon.as_object_mut().unwrap().remove("release_timestamp");
    assert_eq!(
        parse_info(&soon).unwrap().live,
        LiveStatus::IsUpcoming { starts_at: None }
    );
    // Only the flag, no live_status.
    assert_eq!(
        parse_info(&json!({"title": "t", "is_live": true}))
            .unwrap()
            .live,
        LiveStatus::IsLive
    );
    assert_eq!(
        parse_info(&json!({"title": "t", "live_status": "is_live_but_unknown"}))
            .unwrap()
            .live,
        LiveStatus::NotLive
    );
}

#[test]
fn parse_falls_back_and_refuses_nameless() {
    let info = parse_info(&json!({
        "id": "xyz", "original_url": "https://e.example/v/xyz", "channel": "Chan",
        "thumbnails": [{"url": "https://t/1.jpg"}, {"url": "https://t/2.jpg"}],
        "duration": 61.7
    }))
    .unwrap();
    assert_eq!(info.title, "xyz");
    assert_eq!(info.url, "https://e.example/v/xyz");
    assert_eq!(info.uploader.as_deref(), Some("Chan"));
    assert_eq!(info.thumbnail_url.as_deref(), Some("https://t/2.jpg"));
    assert_eq!(info.duration_secs, Some(61));
    assert_eq!(info.extractor, "generic");
    // No formats at all: the playlist-style pair.
    assert_eq!(ids(&info.qualities), ["best", "audio-best"]);
    let err = parse_info(&json!({"formats": []})).unwrap_err();
    assert_eq!(err.kind, ErrorKind::ToolFailed);
}

#[test]
fn qualities_pick_sizes() {
    let formats = fixture("single_video.json")["formats"]
        .as_array()
        .unwrap()
        .clone();
    let options = qualities(&formats);
    assert_eq!(
        ids(&options),
        ["best", "1080p", "720p", "audio-best", "audio-small"]
    );
    // Largest video + the largest audio-only format (251: 3.4 MB).
    assert_eq!(
        option(&options, "best").est_size,
        Some(200_000_000 + 3_400_000)
    );
    assert_eq!(
        option(&options, "1080p").est_size,
        Some(50_000_000 + 3_400_000)
    );
    assert_eq!(
        option(&options, "720p").est_size,
        Some(20_000_000 + 3_400_000)
    );
    assert_eq!(option(&options, "audio-best").est_size, Some(3_400_000));
    assert_eq!(option(&options, "audio-small").est_size, None);
    // The storyboard (999999 bytes, mhtml) counts for nothing.
    assert_eq!(
        option(&options, "1080p").selector,
        "bv*[height<=1080]+ba/b[height<=1080]"
    );
    assert_eq!(
        option(&options, "720p").selector,
        "bv*[height<=720]+ba/b[height<=720]"
    );
    assert_eq!(option(&options, "best").selector, "bv*+ba/b");
    assert!(!option(&options, "best").audio_only);
    assert!(option(&options, "audio-best").audio_only);
    assert_eq!(option(&options, "audio-small").selector, "ba/b");
}

#[test]
fn qualities_only_offer_what_the_formats_allow() {
    let video = |h: u64, size: Option<u64>, acodec: &str| json!({"vcodec": "avc1", "acodec": acodec, "height": h, "filesize": size});
    // 720p at most: no 1080p option; muxed, so no audio is added to the size.
    let options = qualities(&[video(720, Some(10), "mp4a"), video(360, Some(4), "mp4a")]);
    assert_eq!(ids(&options), ["best", "720p", "audio-best", "audio-small"]);
    assert_eq!(option(&options, "best").est_size, Some(10));
    assert_eq!(option(&options, "720p").est_size, Some(10));
    assert_eq!(option(&options, "audio-best").est_size, None);
    // 480p at most: only best.
    let options = qualities(&[video(480, None, "none")]);
    assert_eq!(ids(&options), ["best"]);
    assert_eq!(option(&options, "best").est_size, None);
    // Only 1440p and 1080p exist: a 720p cap would match nothing.
    let options = qualities(&[video(1440, Some(9), "none"), video(1080, Some(5), "none")]);
    assert_eq!(ids(&options), ["best", "1080p"]);
    // Audio only (a music site).
    let audio = json!({"vcodec": "none", "acodec": "opus", "filesize_approx": 1234.0});
    let options = qualities(&[audio]);
    assert_eq!(ids(&options), ["audio-best", "audio-small"]);
    assert_eq!(options[0].est_size, Some(1234));
    // Nothing usable, and no formats: the plain pair.
    assert_eq!(ids(&qualities(&[])), ["best", "audio-best"]);
    let storyboard = json!({"ext": "mhtml", "vcodec": "none", "acodec": "none"});
    assert_eq!(ids(&qualities(&[storyboard])), ["best", "audio-best"]);
}

#[test]
fn audio_options_carry_their_extra_arguments() {
    let options = qualities(
        &fixture("single_video.json")["formats"]
            .as_array()
            .unwrap()
            .clone(),
    );
    assert!(option(&options, "best").extra_args().is_empty());
    assert!(option(&options, "720p").extra_args().is_empty());
    assert_eq!(option(&options, "audio-best").extra_args(), ["-x"]);
    assert_eq!(
        option(&options, "audio-small").extra_args(),
        ["-x", "--audio-format", "mp3", "--audio-quality", "5"]
    );
}

#[test]
fn global_flags_for_every_run() {
    let args = global_args(
        std::path::Path::new("/t/deno/2.5.0/deno"),
        std::path::Path::new("/t/ffmpeg/abc"),
    );
    let args: Vec<_> = args
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        args,
        [
            "--js-runtimes",
            "deno:/t/deno/2.5.0/deno",
            "--ffmpeg-location",
            "/t/ffmpeg/abc",
            "--windows-filenames",
            "--no-warnings",
            "--newline",
            "--no-colors"
        ]
    );
}

#[test]
fn fast_path_hosts() {
    for url in [
        "https://www.youtube.com/watch?v=x",
        "https://youtu.be/x",
        "https://m.youtube.com/watch?v=x",
        "https://music.youtube.com/watch?v=x",
        "http://vimeo.com/1",
        "https://player.vimeo.com/video/1",
        "https://www.twitch.tv/somebody",
        "https://soundcloud.com/a/b",
        "https://x.com/a/status/1",
        "https://twitter.com/a/status/1",
        "https://www.tiktok.com/@a/video/1",
        "https://www.instagram.com/reel/x",
        "https://www.facebook.com/watch?v=1",
        "https://www.dailymotion.com/video/x",
        "https://www.bilibili.com/video/BV1",
        "  https://YouTube.com/watch?v=x \n",
    ] {
        assert!(fast_path_host(url), "{url}");
    }
    for url in [
        "https://example.com/youtube.com",
        "https://notyoutube.com/watch",
        "https://youtube.com.evil.example/watch",
        "ftp://youtube.com/x",
        "magnet:?xt=urn:btih:abc",
        "not a url",
        "https://example.com/file.zip",
    ] {
        assert!(!fast_path_host(url), "{url}");
    }
}

#[test]
fn single_video_links() {
    for url in [
        "https://www.youtube.com/watch?v=abc&list=PL123",
        "https://youtube.com/watch?list=PL123&v=abc&index=2",
        "https://m.youtube.com/watch?v=abc&list=PL123",
        "https://music.youtube.com/watch?v=abc&list=RDAMVMabc",
        "https://youtu.be/abc?list=PL123",
        "https://youtu.be/abc?si=x&list=PL123",
        "  https://www.youtube.com/watch/?v=abc&list=PL123  ",
    ] {
        assert!(single_video_link(url), "{url}");
    }
    for url in [
        // A playlist, or a video alone.
        "https://www.youtube.com/playlist?list=PL123",
        "https://www.youtube.com/watch?v=abc",
        "https://youtu.be/abc",
        "https://www.youtube.com/watch?list=PL123",
        "https://www.youtube.com/watch?v=&list=PL123",
        "https://youtu.be/?list=PL123",
        "https://youtu.be/abc/def?list=PL123",
        "https://www.youtube.com/@channel?v=abc&list=PL123",
        // Other hosts and schemes.
        "https://vimeo.com/watch?v=abc&list=PL123",
        "https://notyoutube.com/watch?v=abc&list=PL123",
        "https://youtube.com.evil.example/watch?v=abc&list=PL123",
        "ftp://youtube.com/watch?v=abc&list=PL123",
        "not a url",
        "",
    ] {
        assert!(!single_video_link(url), "{url}");
    }
}

#[test]
fn playlist_count_skips_unavailable_entries() {
    let json = json!({
        "_type": "playlist",
        "title": "Mix",
        "playlist_count": 5,
        "entries": [{"id": "a"}, null, {"id": "b"}, null, {"id": "c"}],
    });
    let info = parse_info(&json).unwrap();
    assert_eq!(info.playlist.unwrap().count, 3);
    let none = json!({"_type": "playlist", "title": "Gone", "entries": [null, null]});
    assert_eq!(parse_info(&none).unwrap().playlist.unwrap().count, 0);
}

#[test]
fn failure_detail_is_the_error_line() {
    let detail = inspect::failure_detail;
    assert_eq!(
        detail("WARNING: x\nERROR: [youtube] a: Private video\n"),
        "[youtube] a: Private video"
    );
    assert_eq!(detail("just a line\n\n"), "just a line");
    assert_eq!(detail(""), "yt-dlp failed without a message");
}
