//! What the Advanced screens read from the engine: a job's details, size
//! probes streamed as they arrive, and forgetting every saved password.

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use taurus_core::secrets::MemoryStore;
use taurus_core::torrent::hub::TorrentTestNet;
use taurus_core::{
    AddRequest, Credentials, Engine, EngineConfig, JobDetail, JobId, JobState, PowerMode,
    ProgressUnit, Settings, Timing, TorrentSettings,
};
use taurus_testkit::{FileSpec, TestServer, TorrentSeeder, content};

const MIB: usize = 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(30);
const USER: &str = "hyades-detail-user";
const PASSWORD: &str = "d3tail-Pa55-0042";

struct Rig {
    _tmp: tempfile::TempDir,
    dest: PathBuf,
    engine: Engine,
    secrets: Arc<MemoryStore>,
}

impl Rig {
    async fn new(tweak: impl FnOnce(&mut EngineConfig)) -> Rig {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("out");
        fs::create_dir(&dest).unwrap();
        let secrets = Arc::new(MemoryStore::new());
        let mut config = EngineConfig::new(tmp.path().join("data"));
        config.timing = Timing::fast_for_tests();
        config.power = PowerMode::DryRun;
        config.secrets = secrets.clone();
        tweak(&mut config);
        let engine = Engine::open(config).await.unwrap();
        Rig {
            _tmp: tmp,
            dest,
            engine,
            secrets,
        }
    }

    fn request(&self, url: &str) -> AddRequest {
        AddRequest {
            url: url.to_string(),
            dest_dir: self.dest.clone(),
            ..AddRequest::default()
        }
    }

    /// Polls the job's details until `ok`, for up to 30 s.
    async fn detail_until(
        &self,
        id: JobId,
        what: &str,
        ok: impl Fn(&JobDetail) -> bool,
    ) -> JobDetail {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let detail = self.engine.job_detail(id).expect("the job's details");
            if ok(&detail) {
                return detail;
            }
            assert!(Instant::now() < deadline, "never {what}: {detail:?}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

fn creds() -> Option<Credentials> {
    Some(Credentials {
        username: USER.into(),
        password: PASSWORD.into(),
    })
}

fn protected(size: usize) -> FileSpec {
    FileSpec {
        basic_auth: Some((USER.into(), PASSWORD.into())),
        ..FileSpec::new(size)
    }
}

fn done(detail: &JobDetail) -> u64 {
    detail.segments.iter().map(|s| s.done).sum()
}

/// An add that names no connection count gets the settings' one.
#[tokio::test(flavor = "multi_thread")]
async fn settings_connections_are_the_default() {
    let server = TestServer::start().await;
    let url = server.serve(
        "/two.bin",
        FileSpec {
            bytes_per_sec: Some(MIB as u64),
            ..FileSpec::new(8 * MIB)
        },
    );
    let rig = Rig::new(|_| {}).await;
    let settings = Settings {
        connections: 2,
        ..rig.engine.settings()
    };
    rig.engine.set_settings(settings).await.unwrap();
    let id = rig.engine.add(rig.request(&url)).await.unwrap();
    let running = rig
        .detail_until(id, "running", |d| !d.segments.is_empty() && done(d) > 0)
        .await;
    assert_eq!(running.segments.len(), 2, "{running:?}");
    rig.engine.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn job_detail_http_segments() {
    let server = TestServer::start().await;
    let url = server.serve(
        "/big.bin",
        FileSpec {
            bytes_per_sec: Some(2 * MIB as u64),
            ..FileSpec::new(20 * MIB)
        },
    );
    let rig = Rig::new(|_| {}).await;
    let id = rig
        .engine
        .add(AddRequest {
            connections: Some(4),
            ..rig.request(&url)
        })
        .await
        .unwrap();

    let first = rig
        .detail_until(id, "running with segments", |d| {
            d.segments.len() >= 2 && d.connections >= 1 && done(d) > 0
        })
        .await;
    assert_eq!(first.id, id);
    assert!(first.files.is_empty() && first.media_files.is_empty());
    assert!(!first.has_password);
    assert_eq!(first.overrides, None);
    let covered: u64 = first.segments.iter().map(|s| s.end - s.start).sum();
    assert_eq!(covered, (20 * MIB) as u64, "{first:?}");
    let later = rig
        .detail_until(id, "more done", |d| done(d) > done(&first))
        .await;
    assert!(later.segments.len() >= 2);

    // Paused: from the store, with no connections.
    rig.engine.pause(id).await.unwrap();
    let paused = rig.engine.job_detail(id).unwrap();
    assert_eq!(paused.connections, 0);
    assert!(
        paused.segments.len() >= 2 && done(&paused) > 0,
        "{paused:?}"
    );
    assert_eq!(rig.engine.job_detail(JobId(999)), None);
    rig.engine.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn job_detail_torrent_files() {
    let a = content(&FileSpec {
        seed: 41,
        ..FileSpec::new(200 * 1024)
    });
    let b = content(&FileSpec {
        seed: 42,
        ..FileSpec::new(60 * 1024)
    });
    let seeder =
        TorrentSeeder::start("pair", &[("a.bin", a.clone()), ("b.bin", b.clone())], None).await;
    let rig = Rig::new(|c| {
        c.torrent_test = Some(TorrentTestNet {
            initial_peers: vec![seeder.addr],
            restore_in_place: false,
        });
    })
    .await;
    // Sharing stops at once: the torrent completes.
    rig.engine
        .set_settings(Settings {
            torrent: TorrentSettings {
                stop_ratio: Some(0.0),
                ..TorrentSettings::default()
            },
            ..Settings::default()
        })
        .await
        .unwrap();
    let info = rig.engine.torrent_files(&seeder.magnet).await.unwrap();
    let index = |p: &str| info.files.iter().find(|f| f.path == p).unwrap().index;
    let id = rig
        .engine
        .add(AddRequest {
            only_files: Some(vec![index("a.bin")]),
            ..rig.request(&seeder.magnet)
        })
        .await
        .unwrap();

    let detail = rig
        .detail_until(id, "a.bin downloaded", |d| {
            d.files
                .iter()
                .any(|f| f.path == "a.bin" && f.done == a.len() as u64)
        })
        .await;
    assert_eq!(detail.files.len(), 2, "{detail:?}");
    let file = |p: &str| detail.files.iter().find(|f| f.path == p).unwrap();
    assert!(file("a.bin").selected);
    assert!(!file("b.bin").selected);
    assert_eq!(file("a.bin").index, index("a.bin"));
    assert_eq!(file("b.bin").size, b.len() as u64);
    assert!(detail.segments.is_empty());
    assert!(detail.overrides.is_some());

    let state = tokio::time::timeout(TIMEOUT, rig.engine.wait(id))
        .await
        .unwrap();
    assert_eq!(state, JobState::Completed);
    // Finished, out of the session: still listed, from the store.
    let finished = rig.engine.job_detail(id).unwrap();
    let file = |p: &str| finished.files.iter().find(|f| f.path == p).unwrap();
    assert_eq!(file("a.bin").done, a.len() as u64);
    assert!(file("a.bin").selected && !file("b.bin").selected);
    rig.engine.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn job_detail_never_has_password_text() {
    let server = TestServer::start().await;
    let url = server.serve("/locked.bin", protected(64 * 1024));
    let rig = Rig::new(|_| {}).await;
    let id = rig
        .engine
        .add(AddRequest {
            credentials: creds(),
            mirrors: Vec::new(),
            referer: Some("http://ref.example/page".into()),
            ..rig.request(&url)
        })
        .await
        .unwrap();
    assert_eq!(
        tokio::time::timeout(TIMEOUT, rig.engine.wait(id))
            .await
            .unwrap(),
        JobState::Completed
    );
    let detail = rig.engine.job_detail(id).unwrap();
    assert!(detail.has_password);
    assert_eq!(detail.referer.as_deref(), Some("http://ref.example/page"));
    for shown in [
        format!("{detail:?}"),
        format!("{detail:#?}"),
        serde_json::to_string(&detail).unwrap(),
    ] {
        assert!(!shown.contains(PASSWORD), "{shown}");
    }
    rig.engine.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn probe_sizes_stream_yields_all() {
    let server = TestServer::start().await;
    let mut urls: Vec<String> = (1..=4)
        .map(|i| server.serve(&format!("/f{i}.bin"), FileSpec::new(i * 1000)))
        .collect();
    urls.insert(2, format!("{}/missing.bin", server.base_url()));
    let rig = Rig::new(|_| {}).await;

    let mut sizes = rig.engine.probe_sizes_stream(urls.clone());
    let mut got = Vec::new();
    while let Some(result) = tokio::time::timeout(TIMEOUT, sizes.recv()).await.unwrap() {
        got.push(result);
    }
    got.sort();
    assert_eq!(
        got,
        [
            (0, Some(1000)),
            (1, Some(2000)),
            (2, None),
            (3, Some(3000)),
            (4, Some(4000)),
        ]
    );
    // Nothing to probe: the stream ends at once.
    let mut none = rig.engine.probe_sizes_stream(Vec::new());
    assert_eq!(none.recv().await, None);
    rig.engine.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn forget_saved_passwords_removes_all() {
    let server = TestServer::start().await;
    let first = server.serve("/one.bin", protected(10_000));
    let second = server.serve("/two.bin", protected(20_000));
    let rig = Rig::new(|c| c.autostart = false).await;
    let mut ids = Vec::new();
    for url in [&first, &second] {
        let id = rig
            .engine
            .add(AddRequest {
                credentials: creds(),
                ..rig.request(url)
            })
            .await
            .unwrap();
        ids.push(id);
    }
    assert_eq!(rig.secrets.len(), 2);
    assert!(rig.engine.job_detail(ids[0]).unwrap().has_password);

    assert_eq!(rig.engine.forget_saved_passwords().await.unwrap(), 2);
    assert!(rig.secrets.is_empty());
    assert!(!rig.engine.job_detail(ids[0]).unwrap().has_password);
    // Nothing left to remove.
    assert_eq!(rig.engine.forget_saved_passwords().await.unwrap(), 0);

    // A password kept afterwards is tracked again.
    rig.engine
        .set_credentials(ids[1], creds().unwrap())
        .await
        .unwrap();
    assert_eq!(rig.secrets.len(), 1);
    assert!(rig.engine.job_detail(ids[1]).unwrap().has_password);
    assert_eq!(rig.engine.forget_saved_passwords().await.unwrap(), 1);
    assert!(rig.secrets.is_empty());
    rig.engine.shutdown().await.unwrap();
    assert!(rig.engine.forget_saved_passwords().await.is_err());
}

#[test]
fn progress_unit_defaults_to_bytes() {
    assert_eq!(ProgressUnit::default(), ProgressUnit::Bytes);
    assert_eq!(
        serde_json::to_value(ProgressUnit::Millis).unwrap(),
        serde_json::json!("Millis")
    );
}
