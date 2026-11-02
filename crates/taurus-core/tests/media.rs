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
    let err = media::inspect(
        &tools,
        link,
        &Default::default(),
        None,
        Duration::from_millis(700),
    )
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
        MediaRig::with_config(info, download, |_| {}).await
    }

    /// `new`, with `tweak` applied to the engine's config.
    async fn with_config(
        info: &Value,
        download: Value,
        tweak: impl FnOnce(&mut EngineConfig),
    ) -> MediaRig {
        MediaRig::with_scenario(&media_scenario(info, download), tweak).await
    }

    /// `with_config` for a whole scenario of yt-dlp's.
    async fn with_scenario(scenario: &Value, tweak: impl FnOnce(&mut EngineConfig)) -> MediaRig {
        let version = unique_version();
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
        // As the engine records it: no `\\?\` prefix on Windows.
        let dest = dunce::canonicalize(&dest).unwrap();
        let mut config = EngineConfig::new(tmp.path().join("data"));
        config.tool_sources = sources(&host);
        config.timing = Timing::fast_for_tests();
        tweak(&mut config);
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
/// `download` (one scenario, or an array of them told apart by `if_arg`).
fn media_scenario(info: &Value, download: Value) -> Value {
    let mut scenario = vec![json!({ "if_arg": "-J", "stdout": [info.to_string()] })];
    match download {
        Value::Array(entries) => scenario.extend(entries),
        one => scenario.push(one),
    }
    Value::Array(scenario)
}

/// What real yt-dlp prints before an entry's download (`before_dl`):
/// its place in the list, id, title and the file it goes to.
fn entry_line(index: u32, count: u32, id: &str, title: &str, file: &str) -> String {
    format!(
        "TAURUS-ENTRY {index} {count} {} {} {}",
        json!(id),
        json!(title),
        json!(file)
    )
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

/// Whether the paths name the same files, whatever their spelling: a
/// `\\?\` prefix or `/` for `\` on Windows (compared by components).
fn assert_same_paths(got: &[PathBuf], want: &[PathBuf]) {
    let plain = |paths: &[PathBuf]| -> Vec<PathBuf> {
        paths
            .iter()
            .map(|p| dunce::simplified(p).components().collect())
            .collect()
    };
    assert_eq!(plain(got), plain(want));
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
        "stdout": [progress(500, 1000), progress(1000, 1000), file_line(name)],
        // As real yt-dlp prints them: on stderr, under the `--quiet` that
        // `--print` implies, with `pp_key()` names.
        "stderr": ["TAURUS-PP Merger started", "TAURUS-PP Merger finished"],
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
    assert_same_paths(&m.files, &[rig.dest.join(name)]);
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
    let download = json!({
        "stdout": [progress(150, 300), progress(300, 300), file_line("song.mp3")],
        "stderr": [
            "TAURUS-PP ExtractAudio started",
            "TAURUS-PP ExtractAudio processing",
            "TAURUS-PP ExtractAudio finished",
        ],
        "delay_ms_per_line": 30,
        "write_files": [["song.mp3", 300]],
    });
    let rig = MediaRig::new(&video_info(), download).await;
    let mut events = rig.engine.subscribe();
    let link = "https://www.youtube.com/watch?v=audio_only_mp3_args";
    let mut req = rig.request(link);
    req.media = Some(MediaChoice {
        quality: "audio-small".into(),
        convert: Some(ConvertPreset::Mp3),
        ..Default::default()
    });
    let id = rig.engine.add(req).await.unwrap();
    assert_eq!(rig.wait(id).await, JobState::Completed);
    // "Finishing up…": the conversion yt-dlp reports on stderr is seen.
    let mut converting = false;
    while let Ok(event) = events.try_recv() {
        if let Event::JobUpdated(v) = event
            && v.id == id
        {
            converting |= media(&v).phase == MediaPhase::Converting;
        }
    }
    assert!(converting, "no Converting phase");
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
    assert_same_paths(&m.files, &[rig.dest.join(one), rig.dest.join(two)]);
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

/// A live stream that records until stopped, as real yt-dlp records one
/// through ffmpeg: it announces the entry (with the file it goes to, if
/// `report_part`), writes its partial file and prints nothing more, no
/// progress and no finished file. Stopped, Ctrl+C (Unix) ends it with
/// exit 1 (after `slow_exit_ms` per line of a slow way out); on Windows
/// the Ctrl+Break does not reach it and it is killed.
fn live_recording(report_part: bool, slow_exit_ms: u64) -> Value {
    let part = "Live Now [live1].mp4.part";
    let entry = if report_part {
        entry_line(0, 1, "live1", "Live Now", "Live Now [live1].mp4")
    } else {
        format!("TAURUS-ENTRY 0 1 {} {}", json!("live1"), json!("Live Now"))
    };
    json!({
        "stdout": [entry],
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
    assert_same_paths(&media(&view).files, &[file]);
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
async fn a_stop_request_from_another_process_stops_the_recording() {
    // What `taurus stop` does: a file in the data dir, no engine of its own.
    let rig = MediaRig::new(&fixture("live.json"), live_recording(true, 0)).await;
    let link = "https://www.twitch.tv/stop_request_file";
    let id = recording(&rig, link).await;
    let config = EngineConfig::new(rig.tmp.path().join("data"));
    // Not a job id, a missing job and a non-numeric name change nothing.
    let junk = rig.tmp.path().join("data/stop-requests");
    Engine::request_stop(&config, JobId(999)).unwrap();
    std::fs::write(junk.join("junk"), b"").unwrap();

    Engine::request_stop(&config, id).unwrap();
    let view = rig
        .until(id, "completed", |v| v.state == JobState::Completed)
        .await;
    assert_eq!(view.file_name, "Live Now [live1].mp4");
    // Answered means taken: the files are gone.
    assert!(!Engine::stop_request_pending(&config, id));
    assert!(!Engine::stop_request_pending(&config, JobId(999)));
    assert!(junk.join("junk").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_shut_down_engine_leaves_stop_requests_unanswered() {
    let rig = Rig::new(&prints(&fixture("single_video.json"))).await;
    rig.engine.shutdown().await.unwrap();
    let config = EngineConfig::new(rig.tmp.path().join("data"));
    // A shut-down engine does not answer.
    Engine::request_stop(&config, JobId(1)).unwrap();
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert!(Engine::stop_request_pending(&config, JobId(1)));
    Engine::withdraw_stop_request(&config, JobId(1));
    assert!(!Engine::stop_request_pending(&config, JobId(1)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tools_status_is_readable_while_an_engine_holds_the_data_dir() {
    let rig = Rig::new(&prints(&fixture("single_video.json"))).await;
    let config = EngineConfig::new(rig.tmp.path().join("data"));
    let before = Engine::read_tools_status(&config);
    assert!(before.iter().all(|s| s.state == ToolState::Missing));
    rig.engine
        .inspect("https://vimeo.com/read_tools_status")
        .await
        .unwrap();
    // Reading takes no lock, so the open engine does not mind.
    let after = Engine::read_tools_status(&config);
    assert_eq!(after.len(), 3);
    for kind in [ToolKind::YtDlp, ToolKind::Deno] {
        let tool = after.iter().find(|s| s.kind == kind).unwrap();
        assert_eq!(tool.state, ToolState::Ready, "{kind:?}");
        assert!(tool.version.is_some() && tool.path.is_some());
    }
    let ffmpeg = after.iter().find(|s| s.kind == ToolKind::Ffmpeg).unwrap();
    assert_eq!(ffmpeg.state, ToolState::Missing);
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
    // The part was found by its name, not reported: joined, never deleted.
    assert!(rig.dest.join("Live Now [live1].mp4").is_file());
    assert!(rig.dest.join("Live Now [live1].mp4.part").is_file());
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
        dest: dunce::canonicalize(&dest).unwrap(),
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

// ---- Final wave: two-stream recordings, clean stops, removal, limits ----

/// A live stream recorded from its start: yt-dlp downloads the video and
/// the audio side by side, into two partial files it reports in turn.
fn live_two_streams() -> Value {
    let video = "Live Now [live1].f137.mp4.part";
    let audio = "Live Now [live1].f140.m4a.part";
    let line = |part: &str, done: u64| {
        let progress = json!({
            "status": "downloading",
            "downloaded_bytes": done,
            "speed": 2000.0,
            "filename": part.trim_end_matches(".part"),
            "tmpfilename": part,
        });
        format!("TAURUS-PROGRESS {progress}")
    };
    json!({
        "stdout": [
            entry_line(0, 1, "live1", "Live Now", "Live Now [live1].mp4"),
            line(video, 4096),
            line(audio, 1024),
        ],
        "start_files": [[video, 4096], [audio, 1024]],
        "wait_for_interrupt": true,
        "on_interrupt": { "exit": 1 },
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stopped_recording_joins_its_video_and_audio() {
    let rig = MediaRig::new(&fixture("live.json"), live_two_streams()).await;
    // A file of the name the recording would take is someone else's.
    let theirs = rig.dest.join("Live Now [live1].mp4");
    std::fs::write(&theirs, b"not the recording").unwrap();
    let link = "https://www.twitch.tv/joins_video_and_audio";
    let id = recording(&rig, link).await;

    tokio::time::timeout(Duration::from_secs(30), rig.engine.stop_recording(id))
        .await
        .unwrap()
        .unwrap();
    let view = rig.view(id);
    assert_eq!(view.state, JobState::Completed, "{view:?}");
    let file = rig.dest.join("Live Now [live1] (2).mp4");
    // The fake ffmpeg joins its inputs: both streams are in the file.
    assert_eq!(std::fs::metadata(&file).unwrap().len(), 4096 + 1024);
    assert_eq!(view.file_name, "Live Now [live1] (2).mp4");
    assert_same_paths(&media(&view).files, &[file]);
    assert_eq!(std::fs::read(&theirs).unwrap(), b"not the recording");
    let video = rig.dest.join("Live Now [live1].f137.mp4.part");
    let audio = rig.dest.join("Live Now [live1].f140.m4a.part");
    assert!(!video.exists() && !audio.exists(), "the parts are gone");
    let remux = process_args_log()
        .into_iter()
        // This test's own run (the log is shared): by its folder.
        .find(|e| e.tool == "ffmpeg" && e.argv.iter().any(|a| a == video.to_str().unwrap()))
        .expect("ffmpeg ran");
    for run in [
        &["-i", video.to_str().unwrap()][..],
        &["-i", audio.to_str().unwrap()],
        &["-map", "0", "-map", "1", "-c", "copy"],
    ] {
        assert!(has_run(&remux.argv, run), "{run:?} in {:?}", remux.argv);
    }
}

/// An upcoming stream that sends nothing until stopped: yt-dlp waits for
/// it (`--wait-for-video`) before announcing the entry.
fn upcoming_stream() -> Value {
    json!({
        "wait_for_interrupt": true,
        "on_interrupt": { "exit": 1 },
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stopping_before_the_stream_begins_pauses_cleanly() {
    let rig = MediaRig::new(&fixture("live.json"), upcoming_stream()).await;
    let link = "https://www.twitch.tv/stop_before_the_stream";
    let id = rig.engine.add(rig.request(link)).await.unwrap();
    rig.until(id, "waiting for the stream", |v| {
        v.state == JobState::Recording
            && media(v).phase == MediaPhase::WaitingForStream
            && !download_calls(link).is_empty()
    })
    .await;

    tokio::time::timeout(Duration::from_secs(30), rig.engine.stop_recording(id))
        .await
        .unwrap()
        .unwrap();
    let view = rig.view(id);
    assert_eq!(view.state, JobState::Paused, "{view:?}");
    assert_eq!(view.error, None, "an ordinary stop is no failure");
    // Kept, as stored: it can be started again or removed.
    let stored = Engine::read_snapshot(&EngineConfig::new(rig.tmp.path().join("data"))).unwrap();
    let stored = stored.iter().find(|v| v.id == id).unwrap();
    assert_eq!((stored.state, &stored.error), (JobState::Paused, &None));
    rig.engine.resume(id).await.unwrap();
    rig.until(id, "recording again", |v| v.state == JobState::Recording)
        .await;
    rig.engine.remove(id, true).await.unwrap();
    assert!(rig.engine.snapshot().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stopping_during_the_update_after_a_break_pauses_cleanly() {
    let broken = json!({
        "stderr": ["ERROR: [youtube] x: Signature extraction failed: Some exception"],
        "exit": 1,
    });
    let rig = MediaRig::new(&video_info(), broken).await;
    rig.engine.install_tools().await.unwrap();
    // The newer yt-dlp hangs on its version check: the update takes long.
    let hanging = unique_version();
    rig.host.set_release(
        "yt-dlp",
        ToolRelease {
            hang: true,
            ..ToolRelease::new(&hanging)
        },
    );
    let pid_file = taurus_testkit::hang_pid_file(&hanging);
    let link = "https://www.youtube.com/watch?v=stop_during_update";
    let id = rig.engine.add(rig.request(link)).await.unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while !pid_file.exists() {
        assert!(std::time::Instant::now() < deadline, "no update started");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let started = std::time::Instant::now();
    tokio::time::timeout(Duration::from_secs(30), rig.engine.stop_recording(id))
        .await
        .unwrap()
        .unwrap();
    // Not after the 10 s the version check may take.
    assert!(started.elapsed() < Duration::from_secs(5));
    let view = rig.view(id);
    assert_eq!(view.state, JobState::Paused, "{view:?}");
    assert_eq!(view.error, None);
    assert_eq!(download_calls(link).len(), 1);
    // The update was dropped: the tool is not left "installing".
    let tools = rig.engine.tools_status();
    let yt_dlp = tools.iter().find(|s| s.kind == ToolKind::YtDlp).unwrap();
    assert_eq!(yt_dlp.state, ToolState::Ready);
    let _ = std::fs::remove_file(pid_file);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn removing_an_unfinished_media_job_deletes_only_its_own_partial_files() {
    // The audio stream is done (waiting to be merged), the video's part is
    // being written, with yt-dlp's companions of it.
    let own = [
        "Vid [vid1].f140.m4a",
        "Vid [vid1].f137.mp4.part",
        "Vid [vid1].f137.mp4.ytdl",
        "Vid [vid1].f137.mp4.part-Frag3",
    ];
    let download = json!({
        "stdout": [
            format!("TAURUS-ENTRY 0 1 {} {}", json!("vid1"), json!("Vid")),
            progress_into(100, 100, "Vid [vid1].f140.m4a.part", "Vid [vid1].f140.m4a"),
            progress_into(200, 1000, "Vid [vid1].f137.mp4.part", "Vid [vid1].f137.mp4"),
            progress_into(300, 1000, "Vid [vid1].f137.mp4.part", "Vid [vid1].f137.mp4"),
            file_line("Vid [vid1].mp4"),
        ],
        "delay_ms_per_line": 400,
        "start_files": own.iter().map(|f| json!([f, 3])).collect::<Vec<_>>(),
        "write_files": [["Vid [vid1].mp4", 1000]],
        "on_interrupt": { "exit": 1 },
    });
    let rig = MediaRig::new(&video_info(), download).await;
    // Another job's partial file, and the user's files that share the id
    // but are not partial downloads.
    // Files of the same video it did not report writing (another job's,
    // even in a folder inside its own) stay too.
    let decoys = [
        "Other [zzz].f137.mp4.part",
        "Notes [vid1].txt",
        "Song [vid1].flac",
        "Vid [vid1].f251.webm.part",
        "Vid [vid1].temp.mp4",
        "Sub/Vid [vid1].f137.mp4.part",
        "Sub/Other [zzz].mp4.ytdl",
    ];
    std::fs::create_dir_all(rig.dest.join("Sub")).unwrap();
    for decoy in decoys {
        std::fs::write(rig.dest.join(decoy), b"keep").unwrap();
    }
    let link = "https://www.youtube.com/watch?v=remove_partial_files";
    let id = rig.engine.add(rig.request(link)).await.unwrap();
    rig.until(id, "both streams reported", |v| v.downloaded >= 300)
        .await;
    rig.engine.pause(id).await.unwrap();
    for file in own {
        assert!(rig.dest.join(file).is_file(), "{file} written");
    }

    rig.engine
        .remove_with(id, taurus_core::RemoveFiles::IfUnfinished)
        .await
        .unwrap();
    for file in own {
        assert!(!rig.dest.join(file).exists(), "{file} left behind");
    }
    for decoy in decoys {
        assert_eq!(
            std::fs::read(rig.dest.join(decoy)).unwrap(),
            b"keep",
            "{decoy}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn removing_a_finished_media_job_keeps_its_file_unless_asked() {
    let rig = MediaRig::new(&video_info(), downloads("Done [d1].mp4", 100)).await;
    let link = "https://www.youtube.com/watch?v=remove_finished_media";
    let id = rig.engine.add(rig.request(link)).await.unwrap();
    assert_eq!(rig.wait(id).await, JobState::Completed);
    let file = rig.dest.join("Done [d1].mp4");
    rig.engine
        .remove_with(id, taurus_core::RemoveFiles::IfUnfinished)
        .await
        .unwrap();
    assert!(file.is_file(), "a finished file is the user's");

    let id = rig.engine.add(rig.request(link)).await.unwrap();
    assert_eq!(rig.wait(id).await, JobState::Completed);
    rig.engine
        .remove_with(id, taurus_core::RemoveFiles::All)
        .await
        .unwrap();
    assert!(!file.exists(), "asked to delete it");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_speed_limit_reaches_yt_dlp() {
    let rig = MediaRig::new(&video_info(), downloads("limited.mp4", 10)).await;
    let mut settings = rig.engine.settings();
    settings.speed = taurus_core::SpeedLimit::Custom {
        bytes_per_sec: 300_000,
    };
    rig.engine.set_settings(settings).await.unwrap();
    let link = "https://www.youtube.com/watch?v=speed_limit";
    let id = rig.engine.add(rig.request(link)).await.unwrap();
    assert_eq!(rig.wait(id).await, JobState::Completed);
    let argv = &download_calls(link)[0].argv;
    assert!(has_run(argv, &["--limit-rate", "300000"]), "{argv:?}");

    let mut settings = rig.engine.settings();
    settings.speed = taurus_core::SpeedLimit::Fastest;
    rig.engine.set_settings(settings).await.unwrap();
    std::fs::remove_file(rig.dest.join("limited.mp4")).unwrap();
    let link = "https://www.youtube.com/watch?v=speed_fastest";
    let id = rig.engine.add(rig.request(link)).await.unwrap();
    assert_eq!(rig.wait(id).await, JobState::Completed);
    let argv = &download_calls(link)[0].argv;
    assert!(!argv.iter().any(|a| a == "--limit-rate"), "{argv:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_inspected_link_is_not_read_again_on_add() {
    let rig = MediaRig::new(&video_info(), downloads("once.mp4", 10)).await;
    let link = "https://www.youtube.com/watch?v=inspected_once";
    let reads = || {
        calls_with(link)
            .iter()
            .filter(|e| e.argv.iter().any(|a| a == "-J"))
            .count()
    };
    let LinkInfo::Media(info) = rig.engine.inspect(link).await.unwrap() else {
        panic!("not media");
    };
    assert_eq!(reads(), 1);
    let mut req = rig.request(link);
    req.media_info = Some(info);
    let id = rig.engine.add(req).await.unwrap();
    assert_eq!(rig.wait(id).await, JobState::Completed);
    assert_eq!(reads(), 1, "read once");
    // Without it, the link is read.
    std::fs::remove_file(rig.dest.join("once.mp4")).unwrap();
    let id = rig.engine.add(rig.request(link)).await.unwrap();
    assert_eq!(rig.wait(id).await, JobState::Completed);
    assert_eq!(reads(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_video_too_big_for_the_disk_is_refused_on_add() {
    fn little_room(_: &std::path::Path) -> std::io::Result<u64> {
        Ok(1_000)
    }
    let mut info = video_info();
    // Every format sized: the estimate is known.
    for format in info["formats"].as_array_mut().unwrap() {
        format["filesize"] = json!(10_000_000);
    }
    let rig = MediaRig::with_config(&info, downloads("big.mp4", 10), |config| {
        config.available_space = little_room;
    })
    .await;
    let link = "https://www.youtube.com/watch?v=too_big";
    let err = rig.engine.add(rig.request(link)).await.unwrap_err();
    assert!(
        matches!(
            err.kind,
            ErrorKind::DiskFull | ErrorKind::FileTooLargeForDisk
        ),
        "{err}"
    );
    assert!(rig.engine.snapshot().is_empty(), "nothing added");
    // Size unknown (a live stream): no check.
    let rig = MediaRig::with_config(&fixture("live.json"), upcoming_stream(), |config| {
        config.available_space = little_room
    })
    .await;
    let id = rig
        .engine
        .add(rig.request("https://www.twitch.tv/size_unknown"))
        .await
        .unwrap();
    rig.engine.remove(id, false).await.unwrap();
}

/// A progress line of a stream yt-dlp writes to `tmp` (relative to the
/// job's folder, or absolute), to become `file`.
fn progress_into(done: u64, total: u64, tmp: &str, file: &str) -> String {
    let progress = json!({
        "status": "downloading",
        "downloaded_bytes": done,
        "total_bytes": total,
        "speed": 1000.0,
        "tmpfilename": tmp,
        "filename": file,
    });
    format!("TAURUS-PROGRESS {progress}")
}

/// Shuts the rig's engine down and opens a new one on the same data.
async fn reopen(rig: MediaRig) -> MediaRig {
    let MediaRig {
        host,
        tmp,
        engine,
        dest,
    } = rig;
    engine.shutdown().await.unwrap();
    drop(engine);
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn paused_playlist_keeps_finished_videos_on_remove() {
    let info = json!({
        "_type": "playlist",
        "id": "PLK",
        "title": "Kept",
        "webpage_url": "https://www.youtube.com/playlist?list=PLK",
        "extractor_key": "YoutubeTab",
        "entries": [
            {"_type": "url", "id": "k1", "title": "One"},
            {"_type": "url", "id": "k2", "title": "Two"}
        ]
    });
    let one = "Kept/001 - One [k1].mp4";
    let two = "Kept/002 - Two [k2].mp4";
    let two_part = "Kept/002 - Two [k2].mp4.part";
    let download = json!({
        "stdout": [
            format!("TAURUS-ENTRY 1 2 {} {}", json!("k1"), json!("One")),
            progress_into(50, 100, &format!("{one}.part"), one),
            file_line(one),
            format!("TAURUS-ENTRY 2 2 {} {}", json!("k2"), json!("Two")),
            progress_into(30, 100, two_part, two),
        ],
        // Entry 1 is finished; entry 2 is still being downloaded.
        "start_files": [[one, 100], [two_part, 30]],
        "wait_for_interrupt": true,
        "on_interrupt": { "exit": 1 },
    });
    let rig = MediaRig::new(&info, download).await;
    let link = "https://www.youtube.com/playlist?list=PLkeeps_finished";
    let id = rig.engine.add(rig.request(link)).await.unwrap();
    rig.until(id, "entry 2 under way", |v| {
        media(v).entries_done == 1 && v.downloaded >= 130
    })
    .await;
    rig.engine.pause(id).await.unwrap();
    assert_eq!(rig.view(id).state, JobState::Paused);

    rig.engine
        .remove_with(id, taurus_core::RemoveFiles::IfUnfinished)
        .await
        .unwrap();
    assert!(
        rig.dest.join(one).is_file(),
        "a finished video is the user's"
    );
    assert!(!rig.dest.join(two_part).exists(), "entry 2's part is gone");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_jobs_for_the_same_video_keep_each_others_partials() {
    let part = "Same [same1].f137.mp4.part";
    let download = json!({
        "stdout": [
            format!("TAURUS-ENTRY 0 1 {} {}", json!("same1"), json!("Same")),
            progress_into(10, 100, part, "Same [same1].f137.mp4"),
        ],
        // In each job's own folder (yt-dlp runs there).
        "start_files": [[part, 10]],
        "wait_for_interrupt": true,
        "on_interrupt": { "exit": 1 },
    });
    let rig = MediaRig::new(&video_info(), download).await;
    // The same video twice: once in the download folder, once in a folder
    // inside it, and by another link (front ends ask before a duplicate).
    let inner = rig.dest.join("Inner");
    std::fs::create_dir_all(&inner).unwrap();
    let first = rig
        .engine
        .add(rig.request("https://www.youtube.com/watch?v=same_video_1"))
        .await
        .unwrap();
    let mut req = rig.request("https://youtu.be/same_video_2");
    req.dest_dir = inner.clone();
    let second = rig.engine.add(req).await.unwrap();
    for id in [first, second] {
        rig.until(id, "progress", |v| v.downloaded > 0).await;
        rig.engine.pause(id).await.unwrap();
    }
    let (own, other) = (rig.dest.join(part), inner.join(part));
    assert!(own.is_file() && other.is_file());
    // What each job wrote is known after a restart too.
    let rig = reopen(rig).await;

    rig.engine
        .remove_with(first, taurus_core::RemoveFiles::IfUnfinished)
        .await
        .unwrap();
    assert!(!own.exists(), "its own part is gone");
    assert!(other.is_file(), "the other job's part stays");
    rig.engine
        .remove_with(second, taurus_core::RemoveFiles::IfUnfinished)
        .await
        .unwrap();
    assert!(!other.exists());
}

// ---- Real yt-dlp: thumbnails on any container, live without from-start ----

/// What real yt-dlp prints when `--embed-thumbnail` meets a container
/// that cannot hold one (an AVI from archive.org).
const THUMBNAIL_REFUSAL: &str = "ERROR: Postprocessing: Supported filetypes for thumbnail embedding are: mp3, mkv/mka, ogg/opus/flac, m4a/mp4/m4v/mov";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn container_without_thumbnail_support_completes() {
    let info = json!({
        "_type": "playlist",
        "id": "flock",
        "title": "Flock",
        "webpage_url": "https://archive.org/details/flock",
        "extractor_key": "ArchiveOrg",
        "entries": [
            {"_type": "url", "id": "flock/a.avi", "title": "a.avi"},
            {"_type": "url", "id": "flock/b.avi", "title": "b.avi"}
        ]
    });
    // As real yt-dlp does it: each AVI is downloaded and its metadata
    // embedded, then embedding the thumbnail is refused, before the file
    // is reported moved into place; the thumbnail written for it is left,
    // and the run ends with exit 1. (A `/` in an id is `⧸` in the name.)
    let (one, two) = (
        "Flock/001 - a.avi [flock⧸a.avi].avi",
        "Flock/002 - b.avi [flock⧸b.avi].avi",
    );
    let thumbs = [
        "Flock/001 - a.avi [flock⧸a.avi].jpg",
        "Flock/002 - b.avi [flock⧸b.avi].jpg",
    ];
    let download = json!({
        "stdout": [
            entry_line(1, 2, "flock/a.avi", "a.avi", one),
            progress_into(500, 500, &format!("{one}.part"), one),
            entry_line(2, 2, "flock/b.avi", "b.avi", two),
            progress_into(700, 700, &format!("{two}.part"), two),
        ],
        "stderr": [THUMBNAIL_REFUSAL, THUMBNAIL_REFUSAL],
        "write_files": [[one, 500], [two, 700], [thumbs[0], 10], [thumbs[1], 10]],
        "exit": 1,
    });
    let rig = MediaRig::new(&info, download).await;
    // A link read by yt-dlp at once (no page is fetched first).
    let link = "https://www.youtube.com/playlist?list=PLthumbnail_refused";
    let id = rig.engine.add(rig.request(link)).await.unwrap();
    assert_eq!(
        rig.wait(id).await,
        JobState::Completed,
        "{:?}",
        rig.view(id)
    );
    let view = rig.view(id);
    assert_eq!(view.error, None);
    assert_eq!(view.file_name, "Flock");
    assert_eq!(view.total, Some(1200));
    let m = media(&view);
    assert_eq!((m.entries_done, m.entries_total), (2, 2));
    assert_same_paths(&m.files, &[rig.dest.join(one), rig.dest.join(two)]);
    for file in [one, two] {
        assert!(rig.dest.join(file).is_file(), "{file}");
    }
    for thumb in thumbs {
        assert!(!rig.dest.join(thumb).exists(), "{thumb} was left behind");
    }
    // Still asked for: a container that can hold it gets it.
    let argv = &download_calls(link)[0].argv;
    assert!(argv.iter().any(|a| a == "--embed-thumbnail"), "{argv:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_merge_still_fails_beside_a_thumbnail_refusal() {
    let name = "Clip [c1].avi";
    let download = json!({
        "stdout": [entry_line(0, 1, "c1", "Clip", name), progress(100, 100)],
        "stderr": [THUMBNAIL_REFUSAL, "ERROR: Postprocessing: Conversion failed!"],
        "write_files": [[name, 100]],
        "exit": 1,
    });
    let rig = MediaRig::new(&video_info(), download).await;
    let link = "https://www.youtube.com/watch?v=failed_merge_and_thumbnail";
    let id = rig.engine.add(rig.request(link)).await.unwrap();
    assert_eq!(rig.wait(id).await, JobState::Failed);
    let (kind, _) = rig.view(id).error.unwrap();
    assert_eq!(kind, ErrorKind::ToolFailed);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn twitch_live_records_without_live_from_start() {
    // Real yt-dlp on a Twitch stream: `--live-from-start` is refused at
    // once; without it, the stream is recorded from now on.
    let refusal = "ERROR: [twitch:stream] lofigirl: --live-from-start is passed, but there are no formats that can be downloaded from the start. If you want to download from the current time, use --no-live-from-start";
    let download = json!([
        { "if_arg": "--live-from-start", "stderr": [refusal], "exit": 1 },
        live_recording(true, 0),
    ]);
    let rig = MediaRig::new(&fixture("live.json"), download).await;
    let link = "https://www.twitch.tv/records_without_live_from_start";
    let id = recording(&rig, link).await;
    tokio::time::timeout(Duration::from_secs(30), rig.engine.stop_recording(id))
        .await
        .unwrap()
        .unwrap();
    let view = rig.view(id);
    assert_eq!(view.state, JobState::Completed, "{view:?}");
    assert_eq!(view.file_name, "Live Now [live1].mp4");
    let calls = download_calls(link);
    assert_eq!(calls.len(), 2, "{calls:?}");
    let (first, second) = (&calls[0].argv, &calls[1].argv);
    assert!(has_run(
        first,
        &["--live-from-start", "--wait-for-video", "30-600"]
    ));
    assert!(
        !second.iter().any(|a| a == "--live-from-start"),
        "{second:?}"
    );
    // Still waiting for an upcoming stream.
    assert!(
        has_run(second, &["--wait-for-video", "30-600"]),
        "{second:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn killed_live_recording_without_progress_is_joined() {
    // Real yt-dlp recording a live stream: the entry (with the file it
    // goes to), then nothing at all while ffmpeg writes the `.part`. It
    // ignores the interrupt for longer than the grace, so it is killed (as
    // it always is on Windows) and never finalizes the file.
    let part = "Live Now [live1].mp4.part";
    // The thumbnail yt-dlp wrote to embed once it finalizes the file.
    let thumbnail = "Live Now [live1].jpg";
    let download = json!({
        "stdout": [entry_line(0, 1, "live1", "Live Now", "Live Now [live1].mp4")],
        "start_files": [[part, 4096], [thumbnail, 10]],
        "wait_for_interrupt": true,
        "on_interrupt": {
            "stdout": ["finishing", "finishing"],
            "delay_ms_per_line": 20_000,
            "write_files": [["finalized", 1]],
            "exit": 0,
        },
    });
    let rig = MediaRig::new(&fixture("live.json"), download).await;
    let link = "https://www.twitch.tv/killed_without_progress";
    let id = rig.engine.add(rig.request(link)).await.unwrap();
    // Recording: no longer waiting for the stream, the clock runs without
    // a progress line, and the size is the partial file's.
    let view = rig
        .until(id, "two seconds recorded", |v| {
            v.state == JobState::Recording && media(v).live_elapsed_secs >= Some(2)
        })
        .await;
    assert_eq!(media(&view).phase, MediaPhase::Downloading);
    rig.until(id, "the part's size", |v| v.downloaded == 4096)
        .await;

    tokio::time::timeout(Duration::from_secs(30), rig.engine.stop_recording(id))
        .await
        .unwrap()
        .unwrap();
    let view = rig.view(id);
    assert_eq!(view.state, JobState::Completed, "{view:?}");
    assert_eq!(view.error, None);
    let file = rig.dest.join("Live Now [live1].mp4");
    assert_eq!(std::fs::metadata(&file).unwrap().len(), 4096);
    assert_eq!(view.file_name, "Live Now [live1].mp4");
    assert_same_paths(&media(&view).files, &[file]);
    assert!(!rig.dest.join(part).exists(), "the part is joined and gone");
    assert!(!rig.dest.join(thumbnail).exists(), "the thumbnail was left");
    assert!(!rig.dest.join("finalized").exists(), "it was not killed");
    #[cfg(unix)]
    assert!(process_gone(download_calls(link)[0].pid));
    let part = rig.dest.join(part);
    let remux = process_args_log().into_iter().any(|e| {
        e.tool == "ffmpeg" && has_run(&e.argv, &["-y", "-i", part.to_str().unwrap(), "-c", "copy"])
    });
    assert!(remux, "ffmpeg -y -i <part> -c copy <final>.mp4 was run");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_recording_killed_while_yt_dlp_finalizes_it_is_kept() {
    // A long recording stopped: yt-dlp renames its part to the final name,
    // then rewrites the file through `<stem>.temp.mp4` (FixupM3u8, metadata,
    // thumbnail) before reporting it, which takes longer than the grace: it
    // is killed with neither the part nor the report there. (On Windows it
    // is killed before it renames anything: the part is joined.)
    let part = "Live Now [live1].mp4.part";
    let file = "Live Now [live1].mp4";
    let temp = "Live Now [live1].temp.mp4";
    let thumbnail = "Live Now [live1].jpg";
    let download = json!({
        "stdout": [entry_line(0, 1, "live1", "Live Now", file)],
        "start_files": [[part, 4096], [thumbnail, 10]],
        "wait_for_interrupt": true,
        "on_interrupt": {
            "rename_files": [[part, file]],
            "start_files": [[temp, 100]],
            "stdout": ["fixing", "fixing"],
            "delay_ms_per_line": 20_000,
            "exit": 0,
        },
    });
    let rig = MediaRig::new(&fixture("live.json"), download).await;
    let link = "https://www.twitch.tv/killed_while_finalizing";
    let id = recording(&rig, link).await;
    tokio::time::timeout(Duration::from_secs(30), rig.engine.stop_recording(id))
        .await
        .unwrap()
        .unwrap();
    let view = rig.view(id);
    assert_eq!(view.state, JobState::Completed, "{view:?}");
    assert_eq!(view.error, None);
    assert_eq!(view.file_name, file);
    let kept = rig.dest.join(file);
    assert_eq!(std::fs::metadata(&kept).unwrap().len(), 4096);
    assert_same_paths(&media(&view).files, &[kept]);
    for stray in [part, temp, thumbnail] {
        assert!(!rig.dest.join(stray).exists(), "{stray} was left");
    }
    // Made playable by ffmpeg (it may still be MPEG-TS under its name).
    let part = rig.dest.join(part);
    let remux = process_args_log().into_iter().any(|e| {
        e.tool == "ffmpeg" && has_run(&e.argv, &["-y", "-i", part.to_str().unwrap(), "-c", "copy"])
    });
    assert!(remux, "ffmpeg -y -i <part> -c copy <final>.mp4 was run");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_resumed_playlist_leaves_no_thumbnail_of_its_earlier_run() {
    let info = json!({
        "_type": "playlist",
        "id": "flock2",
        "title": "Flock",
        "webpage_url": "https://archive.org/details/flock2",
        "extractor_key": "ArchiveOrg",
        "entries": [
            {"_type": "url", "id": "a", "title": "a"},
            {"_type": "url", "id": "b", "title": "b"}
        ]
    });
    let (one, two) = ("Flock/001 - a [a].avi", "Flock/002 - b [b].avi");
    let (thumb_one, thumb_two) = ("Flock/001 - a [a].jpg", "Flock/002 - b [b].jpg");
    // Paused (Unix: interrupted) after entry 1's thumbnail was written; the
    // next run finds it there and does not write it again.
    let download = json!({
        "stdout": [
            entry_line(1, 2, "a", "a", one),
            progress_into(500, 500, &format!("{one}.part"), one),
            entry_line(2, 2, "b", "b", two),
            progress_into(700, 700, &format!("{two}.part"), two),
        ],
        "stderr": [THUMBNAIL_REFUSAL, THUMBNAIL_REFUSAL],
        "delay_ms_per_line": 300,
        "write_files": [[one, 500], [two, 700], [thumb_two, 10]],
        "exit": 1,
        "on_interrupt": { "write_files": [[thumb_one, 10]], "exit": 1 },
    });
    let rig = MediaRig::new(&info, download).await;
    let link = "https://www.youtube.com/playlist?list=PLresumed_thumbnails";
    let id = rig.engine.add(rig.request(link)).await.unwrap();
    rig.until(id, "progress", |v| v.downloaded > 0).await;
    rig.engine.pause(id).await.unwrap();
    // Well older than the next run's start.
    tokio::time::sleep(Duration::from_millis(2500)).await;
    rig.engine.resume(id).await.unwrap();
    assert_eq!(
        rig.wait(id).await,
        JobState::Completed,
        "{:?}",
        rig.view(id)
    );
    for file in [one, two] {
        assert!(rig.dest.join(file).is_file(), "{file}");
    }
    for thumb in [thumb_one, thumb_two] {
        assert!(!rig.dest.join(thumb).exists(), "{thumb} was left behind");
    }
}

/// A download that fails to sign in, unless it is given a browser's cookies.
fn sign_in_unless_cookies(stderr: &str, name: &str) -> Vec<Value> {
    vec![
        json!({ "if_arg": "--cookies-from-browser", "stdout": [file_line(name)],
                "write_files": [[name, 10]] }),
        json!({ "stderr": [stderr], "exit": 1 }),
    ]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn args_for_cookies_subs_container() {
    let rig = MediaRig::new(&video_info(), downloads("subs.mkv", 10)).await;
    let link = "https://www.youtube.com/watch?v=args_for_cookies_subs";
    let mut req = rig.request(link);
    req.media = Some(taurus_core::MediaChoice {
        quality: "best".into(),
        cookies_from_browser: Some("chrome:Profile 1".into()),
        subtitles: Some(taurus_core::media::SubtitleChoice {
            languages: vec!["en".into(), "vi".into()],
            auto_generated: true,
            embed: true,
        }),
        container: Some(taurus_core::media::Container::Mkv),
        ..Default::default()
    });
    let id = rig.engine.add(req).await.unwrap();
    assert_eq!(
        rig.wait(id).await,
        JobState::Completed,
        "{:?}",
        rig.view(id)
    );
    let calls = download_calls(link);
    assert_eq!(calls.len(), 1, "{calls:?}");
    let argv = &calls[0].argv;
    assert!(
        has_run(
            argv,
            &[
                "-f",
                "bv*+ba/b",
                "--cookies-from-browser",
                "chrome:Profile 1",
                "--write-subs",
                "--write-auto-subs",
                "--sub-langs",
                "en,vi",
                "--embed-subs",
                "--merge-output-format",
                "mkv",
                "--embed-thumbnail",
            ]
        ),
        "{argv:?}"
    );
    // The link was read with the cookies too (the add inspected it itself).
    let inspections: Vec<_> = calls_with(link)
        .into_iter()
        .filter(|e| e.argv.iter().any(|a| a == "-J"))
        .collect();
    assert_eq!(inspections.len(), 1, "{inspections:?}");
    assert!(has_run(
        &inspections[0].argv,
        &["--cookies-from-browser", "chrome:Profile 1"]
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bad_video_choices_are_refused_on_add() {
    let rig = MediaRig::new(&video_info(), downloads("never.mp4", 10)).await;
    let link = "https://www.youtube.com/watch?v=bad_video_choices";
    for choice in [
        taurus_core::MediaChoice {
            cookies_from_browser: Some("netscape".into()),
            ..Default::default()
        },
        taurus_core::MediaChoice {
            subtitles: Some(taurus_core::media::SubtitleChoice {
                languages: vec!["--exec".into()],
                auto_generated: false,
                embed: false,
            }),
            ..Default::default()
        },
        taurus_core::MediaChoice {
            subtitles: Some(taurus_core::media::SubtitleChoice {
                languages: Vec::new(),
                auto_generated: false,
                embed: false,
            }),
            ..Default::default()
        },
    ] {
        let mut req = rig.request(link);
        req.media = Some(choice);
        let err = rig.engine.add(req).await.unwrap_err();
        assert_eq!(err.kind, ErrorKind::Unknown, "{err:?}");
    }
    assert!(rig.engine.snapshot().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retry_with_cookies_requeues() {
    for (stderr, kind) in [
        (
            "ERROR: [youtube] x: Sign in to confirm your age",
            ErrorKind::NeedsSignIn,
        ),
        (
            "ERROR: [youtube] x: Sign in to confirm you\u{2019}re not a bot",
            ErrorKind::BlockedByBotCheck,
        ),
    ] {
        let rig = MediaRig::new(
            &video_info(),
            Value::Array(sign_in_unless_cookies(stderr, "cookie.mp4")),
        )
        .await;
        let link = format!("https://www.youtube.com/watch?v=retry_with_cookies_{kind:?}");
        let id = rig.engine.add(rig.request(&link)).await.unwrap();
        assert_eq!(rig.wait(id).await, JobState::Failed);
        assert_eq!(rig.view(id).error.map(|e| e.0), Some(kind));
        assert!(
            !download_calls(&link)[0]
                .argv
                .iter()
                .any(|a| a.starts_with("--cookies")),
        );

        // A browser that does not exist is refused, and the job is untouched.
        let err = rig.engine.retry_with_cookies(id, "netscape".into()).await;
        assert_eq!(err.unwrap_err().detail, "unknown browser");
        assert_eq!(rig.view(id).state, JobState::Failed);
        // A job that is not a video is refused.
        assert!(
            rig.engine
                .retry_with_cookies(JobId(9999), "firefox".into())
                .await
                .is_err()
        );

        rig.engine
            .retry_with_cookies(id, "firefox".into())
            .await
            .unwrap();
        assert_eq!(
            rig.wait(id).await,
            JobState::Completed,
            "{:?}",
            rig.view(id)
        );
        let calls = download_calls(&link);
        assert_eq!(calls.len(), 2, "{calls:?}");
        assert!(
            has_run(&calls[1].argv, &["--cookies-from-browser", "firefox"]),
            "{:?}",
            calls[1].argv
        );
        assert_eq!(rig.view(id).error, None);
        assert!(rig.dest.join("cookie.mp4").is_file());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retry_with_cookies_refuses_a_job_that_is_not_stopped() {
    let rig = MediaRig::new(&video_info(), downloads("done.mp4", 10)).await;
    let link = "https://www.youtube.com/watch?v=retry_cookies_completed";
    let id = rig.engine.add(rig.request(link)).await.unwrap();
    assert_eq!(rig.wait(id).await, JobState::Completed);
    let err = rig
        .engine
        .retry_with_cookies(id, "firefox".into())
        .await
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Unknown);
    assert_eq!(download_calls(link).len(), 1);
}

/// A video whose reading (`-J`) needs a browser's cookies; its download
/// (told by `--progress`) is let through, so tests look at its arguments.
fn signed_in_only_with_cookies(name: &str) -> Value {
    json!([
        { "if_arg": "--progress", "stdout": [file_line(name)], "write_files": [[name, 10]] },
        { "if_arg": "--cookies-from-browser", "stdout": [video_info().to_string()] },
        { "stderr": ["ERROR: [youtube] x: Sign in to confirm your age"], "exit": 1 },
    ])
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cookies_reach_the_inspection() {
    let rig = MediaRig::with_scenario(&signed_in_only_with_cookies("aged.mp4"), |_| {}).await;
    // No run may write into the source tree: they all run in a temp folder.
    let crate_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let listing = || {
        let mut names: Vec<_> = std::fs::read_dir(crate_dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        names.sort();
        names
    };
    let tree_before = listing();
    let link = "https://www.youtube.com/watch?v=aged_inspection";
    // Without cookies the age check stops the reading, and no job is made.
    let err = rig.engine.inspect(link).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::NeedsSignIn);
    let err = rig.engine.add(rig.request(link)).await;
    // (no media choice: the fast-path host is read by yt-dlp all the same)
    assert_eq!(err.unwrap_err().kind, ErrorKind::NeedsSignIn);
    assert!(rig.engine.snapshot().is_empty());
    // A bad browser is refused before yt-dlp is asked.
    let before = calls_with(link).len();
    let err = rig
        .engine
        .inspect_with_cookies(link, Some("netscape"))
        .await
        .unwrap_err();
    assert_eq!(err.detail, "unknown browser");
    assert_eq!(calls_with(link).len(), before);
    // With cookies the link is read.
    let info = rig
        .engine
        .inspect_with_cookies(link, Some("firefox"))
        .await
        .unwrap();
    assert!(matches!(info, LinkInfo::Media(_)), "{info:?}");
    let last = calls_with(link).pop().unwrap();
    assert!(has_run(&last.argv, &["--cookies-from-browser", "firefox"]));
    // An empty choice is none.
    let err = rig.engine.inspect_with_cookies(link, Some(" ")).await;
    assert_eq!(err.unwrap_err().kind, ErrorKind::NeedsSignIn);

    // Adding with the choice and nothing inspected reads it with the
    // cookies, and the job's runs carry them.
    let mut req = rig.request(link);
    req.media = Some(taurus_core::MediaChoice {
        quality: "best".into(),
        cookies_from_browser: Some("firefox".into()),
        ..Default::default()
    });
    let id = rig.engine.add(req).await.unwrap();
    assert_eq!(
        rig.wait(id).await,
        JobState::Completed,
        "{:?}",
        rig.view(id)
    );
    let calls = calls_with(link);
    let reads: Vec<_> = calls
        .iter()
        .filter(|e| e.argv.iter().any(|a| a == "-J"))
        .collect();
    assert!(
        reads
            .last()
            .is_some_and(|e| has_run(&e.argv, &["--cookies-from-browser", "firefox"]))
    );
    let runs = download_calls(link);
    assert!(has_run(
        &runs.last().unwrap().argv,
        &["--cookies-from-browser", "firefox"]
    ));
    assert_eq!(
        listing(),
        tree_before,
        "a run left a file in the source tree"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn smaller_file_with_mkv_or_webm_is_refused() {
    let rig = MediaRig::new(&video_info(), downloads("never.mp4", 10)).await;
    let link = "https://www.youtube.com/watch?v=smaller_mkv";
    for container in [
        taurus_core::media::Container::Mkv,
        taurus_core::media::Container::Webm,
    ] {
        let mut req = rig.request(link);
        req.media = Some(taurus_core::MediaChoice {
            quality: "best".into(),
            convert: Some(ConvertPreset::SmallerMp4),
            container: Some(container),
            ..Default::default()
        });
        let err = rig.engine.add(req).await.unwrap_err();
        assert!(err.detail.contains("MP4"), "{}", err.detail);
    }
    // Refused before yt-dlp was asked anything.
    assert!(calls_with(link).is_empty());
    // Mp4 with it is fine.
    let mut req = rig.request(link);
    req.media = Some(taurus_core::MediaChoice {
        quality: "best".into(),
        convert: Some(ConvertPreset::SmallerMp4),
        container: Some(taurus_core::media::Container::Mp4),
        ..Default::default()
    });
    rig.engine.add(req).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retry_with_cookies_refuses_a_web_job_and_resumes_a_paused_video() {
    let server = TestServer::start().await;
    let file = server.serve_bytes("/plain.bin", "application/octet-stream", b"0123456789");
    let rig = MediaRig::new(
        &video_info(),
        Value::Array(sign_in_unless_cookies(
            "ERROR: [youtube] x: Sign in to confirm your age",
            "later.mp4",
        )),
    )
    .await;
    let web = rig.engine.add(rig.request(&file)).await.unwrap();
    assert_eq!(rig.wait(web).await, JobState::Completed);
    let err = rig
        .engine
        .retry_with_cookies(web, "firefox".into())
        .await
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Unknown);
    assert!(err.detail.contains("video"), "{}", err.detail);

    // A video that failed and then fails again with the cookies keeps its
    // later runs on them.
    let link = "https://www.youtube.com/watch?v=retry_cookies_twice";
    let id = rig.engine.add(rig.request(link)).await.unwrap();
    assert_eq!(rig.wait(id).await, JobState::Failed);
    rig.engine
        .retry_with_cookies(id, "chrome".into())
        .await
        .unwrap();
    assert_eq!(rig.wait(id).await, JobState::Completed);
    let runs = download_calls(link);
    assert!(has_run(
        &runs[1].argv,
        &["--cookies-from-browser", "chrome"]
    ));
}
