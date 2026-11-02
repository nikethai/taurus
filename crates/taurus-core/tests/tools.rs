//! The engine's tool methods against the testkit's fake publishers.

use std::path::Path;
use std::time::Duration;

use taurus_core::tools::{ToolKind, ToolSources, ToolState, ToolStatus};
use taurus_core::{Engine, EngineConfig, ErrorKind, Event};
use taurus_testkit::{FakeToolHost, ToolRelease};
use tokio::sync::broadcast;

async fn open_engine(data_dir: &Path, host: &FakeToolHost) -> Engine {
    let mut config = EngineConfig::new(data_dir);
    config.tool_sources = ToolSources::under(&url::Url::parse(&host.base_url()).unwrap());
    Engine::open(config).await.unwrap()
}

fn ready(events: &mut broadcast::Receiver<Event>) -> Vec<ToolStatus> {
    let mut statuses = Vec::new();
    while let Ok(event) = events.try_recv() {
        if let Event::ToolStatus(s) = event {
            statuses.push(s);
        }
    }
    statuses
}

#[tokio::test]
async fn engine_installs_updates_and_rolls_back_tools() {
    let host = FakeToolHost::start(
        ToolRelease::new("2026.01.01"),
        ToolRelease::new("7.1"),
        ToolRelease::new("2.5.0"),
    )
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let engine = open_engine(tmp.path(), &host).await;
    assert!(
        engine
            .tools_status()
            .iter()
            .all(|s| s.state == ToolState::Missing)
    );
    let mut events = engine.subscribe();
    tokio::time::timeout(Duration::from_secs(60), engine.install_tools())
        .await
        .unwrap()
        .unwrap();
    let status = engine.tools_status();
    assert_eq!(status.len(), 3);
    assert!(
        status.iter().all(|s| s.state == ToolState::Ready),
        "{status:?}"
    );
    let seen = ready(&mut events);
    for kind in ToolKind::ALL {
        assert!(
            seen.iter()
                .any(|s| s.kind == kind && s.state == ToolState::Ready),
            "{kind:?}: {seen:?}"
        );
    }
    engine.shutdown().await.unwrap();
    drop(engine);

    // The check times were saved: a fresh engine does not check again.
    host.set_release("yt-dlp", ToolRelease::new("2026.02.01"));
    let engine = open_engine(tmp.path(), &host).await;
    let before = host.requests("");
    engine.update_tools(false).await.unwrap();
    assert_eq!(host.requests(""), before);

    let mut events = engine.subscribe();
    engine.update_tools(true).await.unwrap();
    let yt = |e: &Engine| {
        e.tools_status()
            .into_iter()
            .find(|s| s.kind == ToolKind::YtDlp)
            .unwrap()
    };
    assert_eq!(yt(&engine).version.as_deref(), Some("2026.02.01"));
    assert!(
        ready(&mut events)
            .iter()
            .any(|s| s.version.as_deref() == Some("2026.02.01"))
    );

    engine.rollback_tool(ToolKind::YtDlp).await.unwrap();
    assert_eq!(yt(&engine).version.as_deref(), Some("2026.01.01"));
    assert!(
        ready(&mut events)
            .iter()
            .any(|s| s.version.as_deref() == Some("2026.01.01"))
    );
    let err = engine.rollback_tool(ToolKind::Deno).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::ToolMissing);
}

/// Front ends spawn these on a multi-threaded runtime.
fn _tool_futures_are_send(engine: &Engine) {
    fn send<T: Send>(_: T) {}
    send(engine.install_tools());
    send(engine.update_tools(false));
    send(engine.rollback_tool(ToolKind::Deno));
}
