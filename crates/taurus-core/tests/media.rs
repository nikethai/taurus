//! Inspecting media links through the engine, against the testkit's fake
//! publishers and fake yt-dlp.
//!
//! Every test publishes yt-dlp under a version of its own and writes that
//! version's scenario (`taurus_testkit::set_ytdlp_scenario`), so tests in
//! this binary do not share a scenario; the args log is shared, so a test
//! picks its own calls out by its own (unique) link.

use std::time::Duration;

use serde_json::{Value, json};
use taurus_core::media::{self, LiveStatus};
use taurus_core::tools::{ToolKind, ToolManager, ToolSources, ToolState};
use taurus_core::{Engine, EngineConfig, ErrorKind, Event, LinkInfo};
use taurus_testkit::{
    ArgsLogEntry, FakeToolHost, TestServer, ToolRelease, process_args_log, set_ytdlp_scenario,
    unique_version,
};

struct Rig {
    host: FakeToolHost,
    tmp: tempfile::TempDir,
    engine: Engine,
}

impl Rig {
    /// An engine whose tools come from a fake host publishing a yt-dlp that
    /// replays `scenario`.
    async fn new(scenario: &Value) -> Rig {
        let version = unique_version();
        set_ytdlp_scenario(&version, &scenario.to_string());
        let host = FakeToolHost::start(
            ToolRelease::new(&version),
            ToolRelease::new("7.1"),
            ToolRelease::new("2.5.0"),
        )
        .await;
        let tmp = tempfile::tempdir().unwrap();
        let mut config = EngineConfig::new(tmp.path().join("data"));
        config.tool_sources = sources(&host);
        let engine = Engine::open(config).await.unwrap();
        Rig { host, tmp, engine }
    }
}

fn sources(host: &FakeToolHost) -> ToolSources {
    ToolSources::under(&url::Url::parse(&host.base_url()).unwrap())
}

fn fixture(name: &str) -> Value {
    let path = format!("{}/tests/fixtures/ytdlp/{name}", env!("CARGO_MANIFEST_DIR"));
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// A scenario printing `json` on one line, as `-J` does.
fn prints(json: &Value) -> Value {
    json!({ "stdout": [json.to_string()] })
}

fn fails(stderr: &str) -> Value {
    json!({ "stderr": [stderr], "exit": 1 })
}

/// yt-dlp's calls that were given `link`.
fn calls_with(link: &str) -> Vec<ArgsLogEntry> {
    process_args_log()
        .into_iter()
        .filter(|e| e.tool == "yt-dlp" && e.argv.iter().any(|a| a == link))
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn inspect_fast_path_host_uses_ytdlp() {
    let rig = Rig::new(&prints(&fixture("single_video.json"))).await;
    let link = "https://www.youtube.com/watch?v=fast_path_host";
    let info = tokio::time::timeout(Duration::from_secs(60), rig.engine.inspect(link))
        .await
        .unwrap()
        .unwrap();
    let LinkInfo::Media(info) = info else {
        panic!("not media: {info:?}");
    };
    assert_eq!(info.title, "A Fine Video");
    assert_eq!(info.live, LiveStatus::NotLive);
    assert_eq!(info.qualities.len(), 5);

    let calls = calls_with(link);
    assert_eq!(calls.len(), 1, "{calls:?}");
    let argv = &calls[0].argv;
    // The global flags, with the tools the engine installed.
    let tools = rig.engine.tools_status();
    let deno = tools.iter().find(|s| s.kind == ToolKind::Deno).unwrap();
    let at = argv.iter().position(|a| a == "--js-runtimes").unwrap();
    let deno_path = deno.path.as_ref().unwrap().display().to_string();
    assert_eq!(argv[at + 1], format!("deno:{deno_path}"));
    let at = argv.iter().position(|a| a == "--ffmpeg-location").unwrap();
    assert!(
        argv[at + 1].replace('\\', "/").contains("/tools/ffmpeg"),
        "{argv:?}"
    );
    for flag in [
        "--windows-filenames",
        "--no-warnings",
        "--newline",
        "--no-colors",
    ] {
        assert!(argv.iter().any(|a| a == flag), "{flag} in {argv:?}");
    }
    // The inspection itself, ending with the link (after `--`).
    let tail = &argv[argv.len() - 4..];
    assert_eq!(tail, ["--flat-playlist", "-J", "--", link]);
    // Nothing went to the web: the host is on the fast path.
    assert!(rig.tmp.path().join("data/tools/yt-dlp/current").is_file());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tools_are_ensured_once_with_status_events() {
    let rig = Rig::new(&prints(&fixture("single_video.json"))).await;
    let mut events = rig.engine.subscribe();
    let link = "https://vimeo.com/tools_ensured_once";
    rig.engine.inspect(link).await.unwrap();

    let mut seen = Vec::new();
    while let Ok(event) = events.try_recv() {
        if let Event::ToolStatus(s) = event {
            seen.push(s);
        }
    }
    for kind in [ToolKind::YtDlp, ToolKind::Deno] {
        assert!(
            seen.iter()
                .any(|s| s.kind == kind && matches!(s.state, ToolState::Installing { .. })),
            "{kind:?}: {seen:?}"
        );
        assert!(
            seen.iter()
                .any(|s| s.kind == kind && s.state == ToolState::Ready),
            "{kind:?}: {seen:?}"
        );
    }
    // ffmpeg is the download's business, not the inspection's.
    assert!(seen.iter().all(|s| s.kind != ToolKind::Ffmpeg), "{seen:?}");
    assert_eq!(rig.host.requests("ffmpeg-builds/"), 0);

    // A second inspection installs nothing and asks no publisher.
    let before = rig.host.requests("");
    let other = "https://vimeo.com/tools_ensured_once_2";
    rig.engine.inspect(other).await.unwrap();
    assert_eq!(rig.host.requests(""), before);
    assert_eq!(calls_with(other).len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn inspect_page_falls_back_to_media() {
    let rig = Rig::new(&prints(&fixture("playlist.json"))).await;
    let server = TestServer::start().await;
    let page = server.serve_page(
        "/videos/page_falls_back",
        200,
        &[],
        "<html><body><video src=x></video></body></html>",
    );
    let LinkInfo::Media(info) = rig.engine.inspect(&page).await.unwrap() else {
        panic!("not media");
    };
    assert_eq!(info.playlist.map(|p| p.count), Some(3));
    assert_eq!(calls_with(&page).len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn files_and_torrents_never_start_ytdlp() {
    let rig = Rig::new(&prints(&fixture("single_video.json"))).await;
    let server = TestServer::start().await;
    let url = server.serve_bytes("/files/never_ytdlp.bin", "application/octet-stream", b"abc");
    assert!(matches!(
        rig.engine.inspect(&url).await.unwrap(),
        LinkInfo::File { .. }
    ));
    assert!(calls_with(&url).is_empty());
    assert_eq!(rig.host.requests(""), 0, "no tool was installed");
    let err = rig
        .engine
        .inspect("magnet:?xt=urn:btih:nope")
        .await
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::SiteNotSupported);
    assert_eq!(rig.host.requests(""), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unsupported_url_stays_site_not_supported() {
    let unsupported = fails("ERROR: Unsupported URL: https://example.com/blog/post");
    let rig = Rig::new(&unsupported).await;
    // A page on no video host: the web server says "web page", yt-dlp
    // agrees it has no extractor, and the answer is the same as before.
    let server = TestServer::start().await;
    let page = server.serve_page("/blog/unsupported_page", 200, &[], "<html>hi</html>");
    let err = rig.engine.inspect(&page).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::SiteNotSupported);
    assert!(err.detail.contains("web page"), "{}", err.detail);
    assert_eq!(calls_with(&page).len(), 1);
    // And on a fast-path host.
    let link = "https://www.youtube.com/watch?v=unsupported_fast";
    let err = rig.engine.inspect(link).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::SiteNotSupported);
    assert!(err.detail.contains("Unsupported URL"), "{}", err.detail);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn yt_dlp_failures_keep_their_meaning() {
    for (stderr, kind) in [
        (
            "ERROR: [youtube] x: Sign in to confirm you\u{2019}re not a bot",
            ErrorKind::BlockedByBotCheck,
        ),
        (
            "ERROR: [youtube] x: Private video",
            ErrorKind::MediaUnavailable,
        ),
        (
            "ERROR: [youtube] x: Video unavailable",
            ErrorKind::MediaUnavailable,
        ),
        (
            "ERROR: [youtube] x: This video is not available in your country",
            ErrorKind::MediaRegionLocked,
        ),
        (
            "ERROR: [youtube] x: Sign in to confirm your age",
            ErrorKind::NeedsSignIn,
        ),
        ("ERROR: something broke", ErrorKind::Unknown),
    ] {
        let rig = Rig::new(&fails(stderr)).await;
        let err = rig
            .engine
            .inspect("https://www.youtube.com/watch?v=failures")
            .await
            .unwrap_err();
        assert_eq!(err.kind, kind, "{stderr}");
        assert!(!err.detail.starts_with("ERROR:"), "{}", err.detail);
    }
    // Not JSON at all.
    let rig = Rig::new(&json!({"stdout": ["this is not json"]})).await;
    let err = rig
        .engine
        .inspect("https://www.youtube.com/watch?v=not_json")
        .await
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::ToolFailed);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn media_inspect_kills_the_process_group_on_timeout() {
    // A line every 30 s: the fake is still printing when the limit passes.
    let version = unique_version();
    set_ytdlp_scenario(
        &version,
        &json!({"stdout": ["{}", "{}"], "delay_ms_per_line": 30_000}).to_string(),
    );
    let host = FakeToolHost::start(
        ToolRelease::new(&version),
        ToolRelease::new("7.1"),
        ToolRelease::new("2.5.0"),
    )
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let tools = ToolManager::new(tmp.path(), sources(&host), reqwest::Client::new());
    let link = "https://www.youtube.com/watch?v=times_out";
    let started = std::time::Instant::now();
    let err = media::inspect(&tools, link, Duration::from_millis(700))
        .await
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Timeout);
    assert!(
        started.elapsed() < Duration::from_secs(25),
        "{:?}",
        started.elapsed()
    );
    #[cfg(unix)]
    {
        let calls: Vec<_> = process_args_log()
            .into_iter()
            .filter(|e| e.argv.iter().any(|a| a == link))
            .collect();
        let pid = calls[0].pid as libc::pid_t;
        // Killed and reaped: no process of that pid is left.
        assert_ne!(unsafe { libc::kill(pid, 0) }, 0, "yt-dlp is still running");
        // yt-dlp led a process group of its own (pgid = pid), and that group
        // has no member left either.
        assert_ne!(
            unsafe { libc::kill(-pid, 0) },
            0,
            "the process group is still running"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unlisted_page_with_failing_ytdlp_stays_site_not_supported() {
    let server = TestServer::start().await;
    // Whatever yt-dlp complains about, an unlisted page is "a web page".
    for (i, stderr) in [
        "ERROR: [generic] Private video",
        "ERROR: Unable to download webpage: HTTP Error 429: Too Many Requests",
        "ERROR: [generic] Unable to extract title",
        "no error line at all",
    ]
    .into_iter()
    .enumerate()
    {
        let rig = Rig::new(&fails(stderr)).await;
        let page = server.serve_page(
            &format!("/blog/unlisted_failing_{i}"),
            200,
            &[],
            "<html>hi</html>",
        );
        let err = rig.engine.inspect(&page).await.unwrap_err();
        assert_eq!(err.kind, ErrorKind::SiteNotSupported, "{stderr}");
        assert!(err.detail.contains("web page"), "{}", err.detail);
        assert_eq!(calls_with(&page).len(), 1, "yt-dlp was asked: {stderr}");
    }
    // The same complaint on a listed host keeps its meaning.
    let rig = Rig::new(&fails("ERROR: [youtube] x: Private video")).await;
    let err = rig
        .engine
        .inspect("https://www.youtube.com/watch?v=listed_keeps_error")
        .await
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::MediaUnavailable);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unlisted_page_with_tools_offline_stays_site_not_supported() {
    let server = TestServer::start().await;
    let page = server.serve_page("/blog/tools_offline", 200, &[], "<html>hi</html>");
    // A tool host that is not there: yt-dlp cannot be installed.
    let tmp = tempfile::tempdir().unwrap();
    let mut config = EngineConfig::new(tmp.path().join("data"));
    config.tool_sources = ToolSources::under(&url::Url::parse("http://127.0.0.1:9/").unwrap());
    let engine = Engine::open(config).await.unwrap();
    let err = engine.inspect(&page).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::SiteNotSupported);
    assert!(err.detail.contains("web page"), "{}", err.detail);
    // A listed host has no page to fall back to: the real error shows.
    let err = engine
        .inspect("https://www.youtube.com/watch?v=tools_offline_listed")
        .await
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::ConnectionLost, "{err}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn watch_link_with_a_list_is_inspected_as_the_single_video() {
    let rig = Rig::new(&prints(&fixture("single_video.json"))).await;
    let with_list = "https://www.youtube.com/watch?v=one_video&list=PLsingle_video";
    let playlist = "https://www.youtube.com/playlist?list=PLreal_playlist";
    let plain = "https://www.youtube.com/watch?v=no_list";
    for link in [with_list, playlist, plain] {
        rig.engine.inspect(link).await.unwrap();
    }
    let has_flag = |link: &str| {
        let calls = calls_with(link);
        assert_eq!(calls.len(), 1, "{calls:?}");
        let argv = &calls[0].argv;
        // Before the `--`, so yt-dlp reads it as an option.
        let dashes = argv.iter().position(|a| a == "--").unwrap();
        argv[..dashes].iter().any(|a| a == "--no-playlist")
    };
    assert!(has_flag(with_list));
    assert!(!has_flag(playlist));
    assert!(!has_flag(plain));
}

// ---- Media jobs: downloads, pause, live recording, playlists ----

use std::path::PathBuf;

use taurus_core::media::{ConvertPreset, MediaPhase};
use taurus_core::{AddRequest, JobId, JobKind, JobState, JobView, MediaChoice, Timing};

/// An engine (fast timing) whose yt-dlp answers `-J` with `info` and
/// replays `download` for any other run, plus a download folder.
struct MediaRig {
    host: FakeToolHost,
    tmp: tempfile::TempDir,
    engine: Engine,
    dest: PathBuf,
}

impl MediaRig {
    async fn new(info: &Value, download: Value) -> MediaRig {
        let version = unique_version();
        set_ytdlp_scenario(&version, &media_scenario(info, download).to_string());
        let host = FakeToolHost::start(
            ToolRelease::new(&version),
            ToolRelease::new("7.1"),
            ToolRelease::new("2.5.0"),
        )
        .await;
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("dl");
        std::fs::create_dir_all(&dest).unwrap();
        let dest = dest.canonicalize().unwrap();
        let mut config = EngineConfig::new(tmp.path().join("data"));
        config.tool_sources = sources(&host);
        config.timing = Timing::fast_for_tests();
        let engine = Engine::open(config).await.unwrap();
        MediaRig {
            host,
            tmp,
            engine,
            dest,
        }
    }

    fn request(&self, link: &str) -> AddRequest {
        AddRequest {
            url: link.to_owned(),
            dest_dir: self.dest.clone(),
            ..AddRequest::default()
        }
    }

    fn view(&self, id: JobId) -> JobView {
        self.engine
            .snapshot()
            .into_iter()
            .find(|v| v.id == id)
            .unwrap()
    }

    /// Waits up to 30 s for the job's view to satisfy `ok`.
    async fn until(&self, id: JobId, what: &str, ok: impl Fn(&JobView) -> bool) -> JobView {
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        loop {
            let view = self.view(id);
            if ok(&view) {
                return view;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for {what}: {view:?}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    async fn wait(&self, id: JobId) -> JobState {
        tokio::time::timeout(Duration::from_secs(60), self.engine.wait(id))
            .await
            .expect("the job did not finish")
    }
}

/// yt-dlp reading the link (`-J`) prints `info`; any other run replays
/// `download`.
fn media_scenario(info: &Value, download: Value) -> Value {
    json!([{ "if_arg": "-J", "stdout": [info.to_string()] }, download])
}

/// A video's info without its thumbnail (which would be fetched).
fn video_info() -> Value {
    let mut info = fixture("single_video.json");
    info.as_object_mut().unwrap().remove("thumbnail");
    info
}

fn progress(done: u64, total: u64) -> String {
    let progress = json!({
        "status": "downloading",
        "downloaded_bytes": done,
        "total_bytes": total,
        "speed": 1000.0,
        "filename": "x.mp4",
    });
    format!("TAURUS-PROGRESS {progress}")
}

fn file_line(name: &str) -> String {
    format!("TAURUS-FILE {}", json!(name))
}

/// A download that prints some progress, then its file, and writes it.
fn downloads(name: &str, size: u64) -> Value {
    json!({
        "stdout": [progress(size / 2, size), progress(size, size), file_line(name)],
        "write_files": [[name, size]],
    })
}

/// yt-dlp's download runs (not inspections) for `link`.
fn download_calls(link: &str) -> Vec<ArgsLogEntry> {
    calls_with(link)
        .into_iter()
        .filter(|e| !e.argv.iter().any(|a| a == "-J"))
        .collect()
}

fn has_run(argv: &[String], run: &[&str]) -> bool {
    argv.windows(run.len()).any(|w| w == run)
}

/// Whether no process with `pid`, and no process group led by it, is left.
#[cfg(unix)]
fn process_gone(pid: u32) -> bool {
    let pid = pid as libc::pid_t;
    let gone = |target: libc::pid_t| {
        // SAFETY: signal 0 only checks that the target exists.
        let found = unsafe { libc::kill(target, 0) } == 0;
        !found && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
    };
    gone(pid) && gone(-pid)
}

fn media(view: &JobView) -> &taurus_core::MediaView {
    view.media.as_ref().expect("a media view")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn downloads_single_video() {
    let server = TestServer::start().await;
    let thumb = server.serve_bytes("/thumb.jpg", "image/jpeg", b"jpeg bytes");
    let mut info = video_info();
    info["thumbnail"] = json!(thumb);
    let name = "A Fine Video [abc123].mp4";
    let download = json!({
        "stdout": [
            progress(500, 1000),
            progress(1000, 1000),
            "TAURUS-PP Merger started",
            "TAURUS-PP Merger finished",
            file_line(name),
        ],
        "delay_ms_per_line": 30,
        "write_files": [[name, 1000]],
    });
    let rig = MediaRig::new(&info, download).await;
    let mut events = rig.engine.subscribe();
    let link = "https://www.youtube.com/watch?v=downloads_single_video";
    let id = rig.engine.add(rig.request(link)).await.unwrap();
    assert_eq!(rig.wait(id).await, JobState::Completed);

    let view = rig.view(id);
    assert_eq!(view.kind, JobKind::Media);
    assert_eq!(view.file_name, name);
    assert_eq!((view.downloaded, view.total), (1000, Some(1000)));
    assert_eq!(view.error, None);
    let m = media(&view);
    assert_eq!(m.files, [rig.dest.join(name)]);
    assert_eq!((m.entries_done, m.entries_total), (1, 1));
    assert!(rig.dest.join(name).is_file());
    // The phases it went through, in order.
    let mut phases = Vec::new();
    while let Ok(event) = events.try_recv() {
        if let Event::JobUpdated(v) | Event::JobAdded(v) = event
            && v.id == id
            && phases.last() != Some(&media(&v).phase)
        {
            phases.push(media(&v).phase);
        }
    }
    let wanted = [
        MediaPhase::GettingTools,
        MediaPhase::Downloading,
        MediaPhase::Merging,
    ];
    let mut rest = phases.iter();
    assert!(
        wanted.iter().all(|w| rest.any(|p| p == w)),
        "{wanted:?} in order in {phases:?}"
    );
    // Simple defaults, in the brief's order, then the link.
    let calls = download_calls(link);
    assert_eq!(calls.len(), 1, "{calls:?}");
    let argv = &calls[0].argv;
    for run in [
        &["-f", "bv*+ba/b", "--merge-output-format", "mp4"][..],
        &[
            "--embed-thumbnail",
            "--embed-chapters",
            "--embed-metadata",
            "--no-playlist",
        ],
        &[
            "--progress-template",
            "download:TAURUS-PROGRESS %(progress)j",
        ],
        &["--print", "after_move:TAURUS-FILE %(filepath)j"],
    ] {
        assert!(has_run(argv, run), "{run:?} in {argv:?}");
    }
    let template = rig.dest.join("%(title).200B [%(id)s].%(ext)s");
    assert!(
        has_run(argv, &["-o", template.to_str().unwrap()]),
        "{argv:?}"
    );
    assert!(!argv.iter().any(|a| a == "-x" || a == "--live-from-start"));
    assert_eq!(argv.last().unwrap(), link);
    // The thumbnail arrives for the UI.
    let view = rig
        .until(id, "the thumbnail", |v| media(v).thumbnail.is_some())
        .await;
    let thumbnail = media(&view).thumbnail.clone().unwrap();
    assert!(thumbnail.starts_with(rig.tmp.path().join("data/thumbs")));
    assert_eq!(std::fs::read(&thumbnail).unwrap(), b"jpeg bytes");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn audio_only_mp3_args() {
    let rig = MediaRig::new(&video_info(), downloads("song.mp3", 300)).await;
    let link = "https://www.youtube.com/watch?v=audio_only_mp3_args";
    let mut req = rig.request(link);
    req.media = Some(MediaChoice {
        quality: "audio-small".into(),
        convert: Some(ConvertPreset::Mp3),
    });
    let id = rig.engine.add(req).await.unwrap();
    assert_eq!(rig.wait(id).await, JobState::Completed);
    assert_eq!(rig.view(id).file_name, "song.mp3");
    let calls = download_calls(link);
    let argv = &calls[0].argv;
    assert!(
        has_run(argv, &["-f", "ba/b", "-x", "--audio-format", "mp3"]),
        "{argv:?}"
    );
    assert!(
        !argv.iter().any(|a| a == "--merge-output-format"),
        "{argv:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn playlist_progress_counts_entries() {
    let info = json!({
        "_type": "playlist",
        "id": "PL2",
        "title": "Two Songs",
        "webpage_url": "https://www.youtube.com/playlist?list=PL2",
        "extractor_key": "YoutubeTab",
        "entries": [
            {"_type": "url", "id": "a", "title": "One"},
            {"_type": "url", "id": "b", "title": "Two"}
        ]
    });
    let (one, two) = ("Two Songs/001 - One [a].mp4", "Two Songs/002 - Two [b].mp4");
    let download = json!({
        "stdout": [
            format!("TAURUS-ENTRY 1 2 {} {}", json!("a"), json!("One")),
            progress(100, 100),
            file_line(one),
            format!("TAURUS-ENTRY 2 2 {} {}", json!("b"), json!("Two")),
            progress(50, 100),
            file_line(two),
        ],
        "write_files": [[one, 100], [two, 100]],
    });
    let rig = MediaRig::new(&info, download).await;
    let link = "https://www.youtube.com/playlist?list=PLplaylist_progress";
    let id = rig.engine.add(rig.request(link)).await.unwrap();
    assert_eq!(rig.view(id).file_name, "Two Songs");
    assert_eq!(rig.wait(id).await, JobState::Completed);
    let view = rig.view(id);
    let m = media(&view);
    assert_eq!((m.entries_done, m.entries_total), (2, 2));
    assert_eq!(m.files, [rig.dest.join(one), rig.dest.join(two)]);
    assert_eq!(view.file_name, "Two Songs");
    assert_eq!(view.total, Some(200));
    let argv = &download_calls(link)[0].argv;
    assert!(!argv.iter().any(|a| a == "--no-playlist"), "{argv:?}");
    let template = rig
        .dest
        .join("%(playlist_title).100B/%(playlist_index)03d - %(title).150B [%(id)s].%(ext)s");
    assert!(
        has_run(argv, &["-o", template.to_str().unwrap()]),
        "{argv:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn large_playlist_needs_confirmation() {
    let entries: Vec<Value> = (0..51)
        .map(|i| json!({"_type": "url", "id": format!("v{i}"), "title": format!("V{i}")}))
        .collect();
    let info = json!({"_type": "playlist", "id": "PL51", "title": "Big", "entries": entries});
    let rig = MediaRig::new(&info, downloads("Big/001 - V0 [v0].mp4", 10)).await;
    let link = "https://www.youtube.com/playlist?list=PLlarge_playlist";
    let err = rig.engine.add(rig.request(link)).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::NeedsConfirmation);
    assert_eq!(err.detail, "playlist:51");
    assert!(
        rig.engine.snapshot().is_empty(),
        "no job without confirmation"
    );
    let mut req = rig.request(link);
    req.confirm_large_playlist = true;
    let id = rig.engine.add(req).await.unwrap();
    assert_eq!(rig.view(id).kind, JobKind::Media);
    assert_eq!(rig.wait(id).await, JobState::Completed);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pause_interrupts_and_resume_continues() {
    // A slow download that writes its partial file as it starts. Ctrl+C
    // (Unix) ends it with exit 1, as yt-dlp does; on Windows the Ctrl+Break
    // does not reach it and it is killed: either way the part stays.
    let download = json!({
        "stdout": [
            progress(100, 1000),
            progress(200, 1000),
            progress(300, 1000),
            progress(400, 1000),
            file_line("x.mp4"),
        ],
        "delay_ms_per_line": 400,
        "start_files": [["x.mp4.part", 7]],
        "write_files": [["x.mp4", 1000]],
        "on_interrupt": { "write_files": [["interrupted", 1]], "exit": 1 },
    });
    let rig = MediaRig::new(&video_info(), download).await;
    let link = "https://www.youtube.com/watch?v=pause_and_resume";
    let id = rig.engine.add(rig.request(link)).await.unwrap();
    // The first progress line: the fake's interrupt handler is in place.
    rig.until(id, "progress", |v| v.downloaded > 0).await;
    rig.engine.pause(id).await.unwrap();
    let view = rig.view(id);
    assert_eq!(view.state, JobState::Paused);
    assert!(view.downloaded > 0, "progress kept: {view:?}");
    let calls = download_calls(link);
    assert_eq!(calls.len(), 1);
    #[cfg(unix)]
    assert!(
        process_gone(calls[0].pid),
        "yt-dlp (or its group) is still running"
    );
    // On Windows the pid could only be checked with OpenProcess, which the
    // tests do not link; the Job Object kill covers the tree there.
    // yt-dlp's partial file is left for the next run to continue.
    let part = rig.dest.join("x.mp4.part");
    assert_eq!(std::fs::metadata(&part).unwrap().len(), 7);
    // The graceful stop reached yt-dlp (it only can on Unix).
    #[cfg(unix)]
    assert!(rig.dest.join("interrupted").is_file(), "not interrupted");

    rig.engine.resume(id).await.unwrap();
    assert_eq!(rig.wait(id).await, JobState::Completed);
    let calls = download_calls(link);
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].argv, calls[1].argv, "the same run again");
    assert_eq!(rig.view(id).file_name, "x.mp4");
    // Nothing on the engine's side deleted the partial file on the way:
    // yt-dlp got it to continue (the fake leaves it where it was).
    assert!(part.is_file());
}

/// A live stream that records until stopped: it writes its partial file
/// as it starts and never prints a finished one. Stopped, Ctrl+C (Unix)
/// ends it with exit 1 (after `slow_exit_ms` per line of a slow way out);
/// on Windows the Ctrl+Break does not reach it and it is killed.
fn live_recording(report_part: bool, slow_exit_ms: u64) -> Value {
    let part = "Live Now [live1].mp4.part";
    let mut live = json!({
        "status": "downloading",
        "downloaded_bytes": 4096,
        "fragment_index": 3,
        "speed": 2000.0,
    });
    if report_part {
        live["tmpfilename"] = json!(part);
    }
    json!({
        "stdout": [
            format!("TAURUS-ENTRY 0 1 {} {}", json!("live1"), json!("Live Now")),
            format!("TAURUS-PROGRESS {live}"),
        ],
        "start_files": [[part, 4096]],
        "wait_for_interrupt": true,
        "on_interrupt": {
            "stdout": ["bye", "bye"],
            "delay_ms_per_line": slow_exit_ms,
            "write_files": [["interrupted", 1]],
            "exit": 1,
        },
    })
}

async fn recording(rig: &MediaRig, link: &str) -> JobId {
    let id = rig.engine.add(rig.request(link)).await.unwrap();
    let view = rig
        .until(id, "recording", |v| {
            v.state == JobState::Recording && media(v).live_elapsed_secs.is_some()
        })
        .await;
    assert_eq!(media(&view).phase, MediaPhase::Downloading);
    id
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stop_recording_keeps_file() {
    let rig = MediaRig::new(&fixture("live.json"), live_recording(true, 0)).await;
    let link = "https://www.twitch.tv/stop_recording_keeps_file";
    let id = recording(&rig, link).await;

    tokio::time::timeout(Duration::from_secs(30), rig.engine.stop_recording(id))
        .await
        .unwrap()
        .unwrap();
    let view = rig.view(id);
    assert_eq!(view.state, JobState::Completed, "{view:?}");
    let file = rig.dest.join("Live Now [live1].mp4");
    let part = rig.dest.join("Live Now [live1].mp4.part");
    assert_eq!(std::fs::metadata(&file).unwrap().len(), 4096);
    assert!(!part.exists(), "the part is gone");
    assert_eq!(view.file_name, "Live Now [live1].mp4");
    assert_eq!(media(&view).files, [file]);
    #[cfg(unix)]
    assert!(rig.dest.join("interrupted").is_file(), "not interrupted");
    let calls = download_calls(link);
    assert_eq!(calls.len(), 1);
    let argv = &calls[0].argv;
    assert!(
        has_run(argv, &["--live-from-start", "--wait-for-video", "30-600"]),
        "{argv:?}"
    );
    #[cfg(unix)]
    assert!(process_gone(calls[0].pid));
    // The installed ffmpeg made the playable file.
    let remux = process_args_log().into_iter().any(|e| {
        e.tool == "ffmpeg" && has_run(&e.argv, &["-y", "-i", part.to_str().unwrap(), "-c", "copy"])
    });
    assert!(remux, "ffmpeg -y -i <part> -c copy <final>.mp4 was run");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stopped_recording_never_takes_another_jobs_file() {
    // No partial file reported: the folder is searched, by the video's id.
    let rig = MediaRig::new(&fixture("live.json"), live_recording(false, 0)).await;
    let link = "https://www.twitch.tv/never_takes_another_jobs_file";
    let id = recording(&rig, link).await;
    // Another job's recording in the same folder, newer than this one's.
    let decoy = rig.dest.join("Other Stream [zzz].mp4.part");
    std::fs::write(&decoy, b"someone else's").unwrap();
    let decoy_ts = rig.dest.join("Old Stream.ts");
    std::fs::write(&decoy_ts, b"an old one").unwrap();

    tokio::time::timeout(Duration::from_secs(30), rig.engine.stop_recording(id))
        .await
        .unwrap()
        .unwrap();
    let view = rig.view(id);
    assert_eq!(view.state, JobState::Completed, "{view:?}");
    assert_eq!(view.file_name, "Live Now [live1].mp4");
    assert_eq!(std::fs::read(&decoy).unwrap(), b"someone else's");
    assert_eq!(std::fs::read(&decoy_ts).unwrap(), b"an old one");
    assert!(!rig.dest.join("Other Stream [zzz].mp4").exists());
    assert!(!rig.dest.join("Old Stream.mp4").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_engine_answers_while_a_recording_stops() {
    // yt-dlp takes about 1.5 s to stop (a 3 s grace lets it).
    let version = unique_version();
    let scenario = media_scenario(&fixture("live.json"), live_recording(true, 750));
    set_ytdlp_scenario(&version, &scenario.to_string());
    let host = FakeToolHost::start(
        ToolRelease::new(&version),
        ToolRelease::new("7.1"),
        ToolRelease::new("2.5.0"),
    )
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let dest = tmp.path().join("dl");
    std::fs::create_dir_all(&dest).unwrap();
    let mut config = EngineConfig::new(tmp.path().join("data"));
    config.tool_sources = sources(&host);
    config.timing = Timing {
        media_stop_grace: Duration::from_secs(3),
        ..Timing::fast_for_tests()
    };
    let rig = MediaRig {
        host,
        engine: Engine::open(config).await.unwrap(),
        dest: dest.canonicalize().unwrap(),
        tmp,
    };
    let id = recording(&rig, "https://www.twitch.tv/answers_while_stopping").await;

    let engine = rig.engine.clone();
    let stopping = tokio::spawn(async move { engine.stop_recording(id).await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    // Only on Unix does the interrupt reach yt-dlp and make its slow way
    // out run; on Windows it may be killed at once.
    #[cfg(unix)]
    assert!(!stopping.is_finished(), "the stop took no time at all");
    let started = std::time::Instant::now();
    let mut settings = rig.engine.settings();
    settings.max_active = 2;
    rig.engine.set_settings(settings).await.unwrap();
    assert!(
        started.elapsed() < Duration::from_millis(700),
        "a command waited for the stop: {:?}",
        started.elapsed()
    );
    #[cfg(unix)]
    assert!(!stopping.is_finished(), "answered before the stop ended");
    tokio::time::timeout(Duration::from_secs(30), stopping)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(rig.view(id).state, JobState::Completed);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn extractor_break_updates_and_retries() {
    let broken = json!({
        "stderr": ["ERROR: [youtube] x: Signature extraction failed: Some exception"],
        "exit": 1,
    });
    let rig = MediaRig::new(&video_info(), broken).await;
    rig.engine.install_tools().await.unwrap();
    let old = yt_dlp_version(&rig.engine);
    // A newer yt-dlp is published, and it copes with the site.
    let newer = unique_version();
    set_ytdlp_scenario(
        &newer,
        &media_scenario(&video_info(), downloads("fixed.mp4", 50)).to_string(),
    );
    rig.host.set_release("yt-dlp", ToolRelease::new(&newer));

    let link = "https://www.youtube.com/watch?v=extractor_break";
    let id = rig.engine.add(rig.request(link)).await.unwrap();
    assert_eq!(
        rig.wait(id).await,
        JobState::Completed,
        "{:?}",
        rig.view(id)
    );
    assert_eq!(rig.view(id).file_name, "fixed.mp4");
    assert_eq!(download_calls(link).len(), 2, "exactly one automatic retry");
    assert_eq!(yt_dlp_version(&rig.engine), newer);
    assert_ne!(old, newer);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn jobs_breaking_together_each_retry_once() {
    // Both runs are still going when the first one's update lands, so the
    // second one's own forced update finds nothing newer to install.
    let broken = json!({
        "stdout": [progress(1, 10)],
        "delay_ms_per_line": 1500,
        "stderr": ["ERROR: [youtube] x: Signature extraction failed: Some exception"],
        "exit": 1,
    });
    let rig = MediaRig::new(&video_info(), broken).await;
    rig.engine.install_tools().await.unwrap();
    let newer = unique_version();
    // Each job writes a file of its own name: the link's last letter.
    let fixed = json!([
        { "if_arg": "-J", "stdout": [video_info().to_string()] },
        { "if_arg": "https://www.youtube.com/watch?v=together_a",
          "stdout": [file_line("a.mp4")], "write_files": [["a.mp4", 5]] },
        { "if_arg": "https://www.youtube.com/watch?v=together_b",
          "stdout": [file_line("b.mp4")], "write_files": [["b.mp4", 5]] },
    ]);
    set_ytdlp_scenario(&newer, &fixed.to_string());
    rig.host.set_release("yt-dlp", ToolRelease::new(&newer));

    let (a, b) = (
        "https://www.youtube.com/watch?v=together_a",
        "https://www.youtube.com/watch?v=together_b",
    );
    let first = rig.engine.add(rig.request(a)).await.unwrap();
    let second = rig.engine.add(rig.request(b)).await.unwrap();
    for (id, link) in [(first, a), (second, b)] {
        assert_eq!(
            rig.wait(id).await,
            JobState::Completed,
            "{:?}",
            rig.view(id)
        );
        assert_eq!(download_calls(link).len(), 2, "{link}: exactly one retry");
    }
    assert_eq!(yt_dlp_version(&rig.engine), newer);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn extractor_break_without_a_newer_yt_dlp_fails() {
    let broken = json!({
        "stderr": ["ERROR: [youtube] x: nsig extraction failed: Some exception"],
        "exit": 1,
    });
    let rig = MediaRig::new(&video_info(), broken).await;
    let link = "https://www.youtube.com/watch?v=extractor_break_no_update";
    let id = rig.engine.add(rig.request(link)).await.unwrap();
    assert_eq!(rig.wait(id).await, JobState::Failed);
    let view = rig.view(id);
    let (kind, detail) = view.error.unwrap();
    assert_eq!(kind, ErrorKind::Unknown);
    assert!(detail.contains("nsig extraction failed"), "{detail}");
    assert_eq!(download_calls(link).len(), 1, "no retry without an update");
}

fn yt_dlp_version(engine: &Engine) -> String {
    let tools = engine.tools_status();
    let yt_dlp = tools.iter().find(|s| s.kind == ToolKind::YtDlp).unwrap();
    yt_dlp.version.clone().unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn region_locked_fails_with_message_kind() {
    let blocked = json!({
        "stderr": [
            "ERROR: [youtube] x: The uploader has not made this video available in your country"
        ],
        "exit": 1,
    });
    let rig = MediaRig::new(&video_info(), blocked).await;
    let link = "https://www.youtube.com/watch?v=region_locked";
    let id = rig.engine.add(rig.request(link)).await.unwrap();
    assert_eq!(rig.wait(id).await, JobState::Failed);
    let (kind, detail) = rig.view(id).error.unwrap();
    assert_eq!(kind, ErrorKind::MediaRegionLocked);
    assert!(!detail.starts_with("ERROR:"), "{detail}");
    assert_eq!(download_calls(link).len(), 1);
    assert!(!kind.is_transient());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn media_job_waits_for_tools() {
    let rig = MediaRig::new(&video_info(), downloads("later.mp4", 20)).await;
    let link = "https://www.youtube.com/watch?v=waits_for_tools";
    // Reading the link installs yt-dlp and Deno; ffmpeg is the download's.
    rig.engine.inspect(link).await.unwrap();
    rig.host.go_offline();
    let id = rig.engine.add(rig.request(link)).await.unwrap();
    let view = rig
        .until(id, "waiting for the tools", |v| {
            v.state == JobState::Waiting
        })
        .await;
    assert_eq!(
        view.error.as_ref().map(|e| e.0),
        Some(ErrorKind::ConnectionLost)
    );
    assert_eq!(media(&view).phase, MediaPhase::GettingTools);
    assert!(view.retry_at.is_some());
    assert!(download_calls(link).is_empty(), "no run without the tools");

    rig.host.go_online();
    assert_eq!(
        rig.wait(id).await,
        JobState::Completed,
        "{:?}",
        rig.view(id)
    );
    assert!(rig.dest.join("later.mp4").is_file());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn media_jobs_count_against_max_active_and_survive_a_restart() {
    let download = json!({
        "stdout": [progress(1, 2), progress(2, 2), file_line("slow.mp4")],
        "delay_ms_per_line": 300,
        "write_files": [["slow.mp4", 2]],
        "on_interrupt": { "exit": 1 },
    });
    let rig = MediaRig::new(&video_info(), download).await;
    let mut settings = rig.engine.settings();
    settings.max_active = 1;
    rig.engine.set_settings(settings).await.unwrap();
    let first = rig
        .engine
        .add(rig.request("https://www.youtube.com/watch?v=max_active_1"))
        .await
        .unwrap();
    let second = rig
        .engine
        .add(rig.request("https://www.youtube.com/watch?v=max_active_2"))
        .await
        .unwrap();
    rig.until(first, "running", |v| v.state == JobState::Downloading)
        .await;
    assert_eq!(rig.view(second).state, JobState::Queued);
    // A shutdown interrupts the run; the next open runs it again.
    rig.engine.shutdown().await.unwrap();
    let stored = Engine::read_snapshot(&EngineConfig::new(rig.tmp.path().join("data"))).unwrap();
    let first_stored = stored.iter().find(|v| v.id == first).unwrap();
    assert_eq!(first_stored.state, JobState::Queued);
    assert_eq!(first_stored.kind, JobKind::Media);
    assert!(first_stored.media.is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_daily_yt_dlp_check_runs_with_a_media_job() {
    let mut rig = MediaRig::new(&video_info(), downloads("daily.mp4", 10)).await;
    rig.engine.install_tools().await.unwrap();
    let newer = unique_version();
    set_ytdlp_scenario(
        &newer,
        &media_scenario(&video_info(), downloads("daily.mp4", 10)).to_string(),
    );
    rig.host.set_release("yt-dlp", ToolRelease::new(&newer));
    let checks = || rig.host.requests("api/repos/yt-dlp");
    let before = checks();

    // Installed today: the job does not ask again.
    let link = "https://www.youtube.com/watch?v=daily_check_today";
    let id = rig.engine.add(rig.request(link)).await.unwrap();
    assert_eq!(rig.wait(id).await, JobState::Completed);
    assert_eq!(checks(), before, "checked within 24 h of the install");
    assert_ne!(yt_dlp_version(&rig.engine), newer);

    // A day later, the first media job checks and gets the newer one.
    rig.engine.shutdown().await.unwrap();
    let data = rig.tmp.path().join("data");
    let store = taurus_core::store::Store::open(&data.join("taurus.db")).unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    store
        .save_tool_checked_at("yt-dlp", now - 25 * 3600)
        .unwrap();
    drop(store);
    let mut config = EngineConfig::new(&data);
    config.tool_sources = sources(&rig.host);
    config.timing = Timing::fast_for_tests();
    rig.engine = Engine::open(config).await.unwrap();
    let link = "https://www.youtube.com/watch?v=daily_check_tomorrow";
    std::fs::remove_file(rig.dest.join("daily.mp4")).unwrap();
    let id = rig.engine.add(rig.request(link)).await.unwrap();
    assert_eq!(rig.wait(id).await, JobState::Completed);
    assert_eq!(checks(), before + 1);
    assert_eq!(yt_dlp_version(&rig.engine), newer);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn engine_futures_stay_small() {
    // Front ends await these on their main thread, whose stack is 1 MiB on
    // Windows; debug builds copy a future a few times on the way.
    let tmp = tempfile::tempdir().unwrap();
    let engine = Engine::open(EngineConfig::new(tmp.path())).await.unwrap();
    let id = JobId(1);
    let limit = 16 * 1024;
    let sizes = [
        (
            "add",
            std::mem::size_of_val(&engine.add(AddRequest::default())),
        ),
        ("inspect", std::mem::size_of_val(&engine.inspect("x"))),
        ("resume", std::mem::size_of_val(&engine.resume(id))),
        (
            "stop_recording",
            std::mem::size_of_val(&engine.stop_recording(id)),
        ),
        (
            "install_tools",
            std::mem::size_of_val(&engine.install_tools()),
        ),
        (
            "update_tools",
            std::mem::size_of_val(&engine.update_tools(false)),
        ),
    ];
    for (name, size) in sizes {
        assert!(size < limit, "Engine::{name} is a {size}-byte future");
    }
}
