//! The manual proxy: checked when it is set, and used by every web request
//! (downloads, grab, tool installs) and by yt-dlp, from the next request on.
//!
//! The links name `unresolvable.invalid` (`.invalid` never resolves), so a
//! request only succeeds if it went through the proxy. The proxies are
//! plain HTTP: the testkit has no TLS, so HTTPS through a proxy (CONNECT)
//! is not exercised here.

use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use taurus_core::network::ProxySetting;
use taurus_core::secrets::MemoryStore;
use taurus_core::tools::ToolSources;
use taurus_core::{AddRequest, Engine, EngineConfig, ErrorKind, GrabOptions, JobState, LinkInfo};
use taurus_testkit::{
    FakeToolHost, FileSpec, TestServer, ToolRelease, content, process_args_log, set_ytdlp_scenario,
    sha256_hex, unique_version,
};

const NOWHERE: &str = "http://unresolvable.invalid";
const TIMEOUT: Duration = Duration::from_secs(60);

fn config(tmp: &tempfile::TempDir) -> EngineConfig {
    let mut config = EngineConfig::new(tmp.path().join("data"));
    config.secrets = Arc::new(MemoryStore::new());
    config
}

fn manual(url: &str) -> ProxySetting {
    ProxySetting::Manual {
        url: url.to_owned(),
    }
}

async fn set_proxy(engine: &Engine, proxy: ProxySetting) {
    let mut settings = engine.settings();
    settings.network.proxy = proxy;
    engine.set_settings(settings).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bad_proxy_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let engine = Engine::open(config(&tmp)).await.unwrap();
    set_proxy(&engine, manual("socks5h://127.0.0.1:9050")).await;
    let before = engine.settings();
    for bad in [
        "nope",
        "ftp://proxy.example:21",
        "localhost:8080",
        "http://",
    ] {
        let mut settings = before.clone();
        settings.network.proxy = manual(bad);
        // Something else changes too: nothing is applied.
        settings.max_active = 7;
        let e = engine.set_settings(settings).await.unwrap_err();
        assert_eq!(e.kind, ErrorKind::Unknown, "{bad}");
        assert!(e.detail.starts_with("unsupported proxy"), "{bad}: {e}");
        assert_eq!(engine.settings(), before, "{bad}");
    }
    engine.shutdown().await.unwrap();
    // Nor saved.
    let engine = Engine::open(config(&tmp)).await.unwrap();
    assert_eq!(engine.settings(), before);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn proxy_applies_everywhere() {
    let tmp = tempfile::tempdir().unwrap();
    let dest = tmp.path().join("out");
    std::fs::create_dir(&dest).unwrap();
    let proxy = TestServer::start_proxy().await;
    let spec = FileSpec::new(300_000);
    proxy.serve("/x.bin", spec.clone());
    proxy.serve_page(
        "/page/",
        200,
        &[],
        r#"<html><body><a href="a.zip">a</a> <a href="b.pdf">b</a></body></html>"#,
    );
    let version = unique_version();
    let fixture = format!(
        "{}/tests/fixtures/ytdlp/single_video.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let info: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(fixture).unwrap()).unwrap();
    set_ytdlp_scenario(
        &version,
        &json!({ "stdout": [info.to_string()] }).to_string(),
    );
    let host = FakeToolHost::start(
        ToolRelease::new(&version),
        ToolRelease::new("7.1"),
        ToolRelease::new("2.5.0"),
    )
    .await;
    let mut config = config(&tmp);
    // Tools come from a host that only the proxy can reach.
    config.tool_sources = ToolSources::under(&url::Url::parse(&format!("{NOWHERE}/")).unwrap());
    let engine = Engine::open(config).await.unwrap();
    // The engine opened with the system settings; the proxy applies from
    // the next request on.
    set_proxy(&engine, manual(proxy.base_url())).await;

    // A download: its probe and its transfer.
    let id = engine
        .add(AddRequest {
            url: format!("{NOWHERE}/x.bin"),
            dest_dir: dest.clone(),
            ..AddRequest::default()
        })
        .await
        .unwrap();
    let state = tokio::time::timeout(TIMEOUT, engine.wait(id))
        .await
        .unwrap();
    assert_eq!(state, JobState::Completed);
    let got = std::fs::read(dest.join("x.bin")).unwrap();
    assert_eq!(sha256_hex(&got), sha256_hex(&content(&spec)));
    let proxied = proxy.proxied_requests();
    assert!(
        proxied.iter().any(|r| r == &format!("{NOWHERE}/x.bin")),
        "{proxied:?}"
    );

    // Grab, and its size probes.
    let page = format!("{NOWHERE}/page/");
    let result = engine
        .grab(&page, GrabOptions { subfolders: false })
        .await
        .unwrap();
    let names: Vec<_> = result.links.iter().map(|l| l.name.as_str()).collect();
    assert_eq!(names, ["a.zip", "b.pdf"]);
    assert!(proxy.proxied_requests().contains(&page));
    let sizes = engine.probe_sizes(vec![format!("{NOWHERE}/x.bin")]).await;
    assert_eq!(sizes, [Some(300_000)]);

    // Another proxy, at once: tool installs (version lookups, downloads,
    // checksums) go through it. The tool host serves whatever path an
    // absolute-form request names.
    let tool_proxy = host.base_url();
    set_proxy(&engine, manual(&tool_proxy)).await;
    let before = proxy.proxied_requests().len();
    tokio::time::timeout(TIMEOUT, engine.install_tools())
        .await
        .unwrap()
        .unwrap();
    assert!(host.requests("api/repos/yt-dlp") >= 1);
    assert!(host.requests("yt-dlp/") >= 2, "the binary and its sums");
    assert_eq!(proxy.proxied_requests().len(), before);

    // yt-dlp is told the proxy.
    let link = "https://www.youtube.com/watch?v=proxy_applies";
    let info = tokio::time::timeout(TIMEOUT, engine.inspect(link))
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(info, LinkInfo::Media(_)), "{info:?}");
    let calls: Vec<_> = process_args_log()
        .into_iter()
        .filter(|e| e.tool == "yt-dlp" && e.argv.iter().any(|a| a == link))
        .collect();
    assert_eq!(calls.len(), 1, "{calls:?}");
    let argv = &calls[0].argv;
    let at = argv.iter().position(|a| a == "--proxy").expect("--proxy");
    assert_eq!(argv[at + 1], tool_proxy);
    // Before the link, among the global flags.
    assert!(at < argv.iter().position(|a| a == "--").unwrap());

    // Direct connections: yt-dlp is told so too.
    set_proxy(&engine, ProxySetting::None).await;
    let direct = "https://www.youtube.com/watch?v=proxy_none";
    tokio::time::timeout(TIMEOUT, engine.inspect(direct))
        .await
        .unwrap()
        .unwrap();
    let calls: Vec<_> = process_args_log()
        .into_iter()
        .filter(|e| e.tool == "yt-dlp" && e.argv.iter().any(|a| a == direct))
        .collect();
    let argv = &calls[0].argv;
    let at = argv.iter().position(|a| a == "--proxy").expect("--proxy");
    assert_eq!(argv[at + 1], "");
    // And the web requests no longer reach the proxy: the host is unknown.
    let before = proxy.proxied_requests().len();
    let sizes = engine.probe_sizes(vec![format!("{NOWHERE}/x.bin")]).await;
    assert_eq!(sizes, [None]);
    assert_eq!(proxy.proxied_requests().len(), before);
}
