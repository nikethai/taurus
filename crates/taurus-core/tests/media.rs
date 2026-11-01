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
        ("ERROR: [youtube] x: Private video", ErrorKind::NeedsSignIn),
        (
            "ERROR: [youtube] x: Video unavailable",
            ErrorKind::LinkExpired,
        ),
        ("ERROR: something broke", ErrorKind::ToolFailed),
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
async fn media_inspect_kills_yt_dlp_on_timeout() {
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
    }
}
