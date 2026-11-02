//! Changing a torrent job's file selection after it was added, and its own
//! sharing rules, against real local librqbit sessions on 127.0.0.1.

use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

use sha2::{Digest, Sha256};
use taurus_core::store::JobId;
use taurus_core::system::SpaceSource;
use taurus_core::system::space::SPACE_MARGIN;
use taurus_core::torrent::hub::TorrentTestNet;
use taurus_core::{
    AddRequest, Engine, EngineConfig, ErrorKind, JobState, JobView, PowerMode, Settings, Timing,
    TorrentOverrides, TorrentSettings,
};
use taurus_testkit::{FileSpec, TorrentSeeder, content};
use tempfile::TempDir;

const TIMEOUT: Duration = Duration::from_secs(30);

/// The free space every engine of this file sees; `u64::MAX` is plenty.
static FREE: AtomicU64 = AtomicU64::new(u64::MAX);

fn free_space(_: &Path) -> std::io::Result<u64> {
    Ok(FREE.load(Ordering::SeqCst))
}

fn data(size: usize, seed: u64) -> Vec<u8> {
    content(&FileSpec {
        seed,
        ..FileSpec::new(size)
    })
}

/// Seeds `pack/a.bin` (300 KiB) and `pack/dir/b.bin` (100 KiB).
async fn seeder() -> (TorrentSeeder, Vec<u8>, Vec<u8>) {
    let a = data(300 * 1024, 31);
    let b = data(100 * 1024, 32);
    let seeder = TorrentSeeder::start(
        "pack",
        &[("a.bin", a.clone()), ("dir/b.bin", b.clone())],
        None,
    )
    .await;
    (seeder, a, b)
}

struct Rig {
    _tmp: TempDir,
    dest: PathBuf,
    engine: Engine,
}

impl Rig {
    async fn new(peers: Vec<SocketAddr>, space: SpaceSource, torrent: TorrentSettings) -> Rig {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("dest");
        fs::create_dir_all(&dest).unwrap();
        let mut config = EngineConfig::new(tmp.path().join("data"));
        config.timing = Timing::fast_for_tests();
        config.power = PowerMode::DryRun;
        config.available_space = space;
        config.torrent_test = Some(TorrentTestNet {
            initial_peers: peers,
            restore_in_place: false,
        });
        let engine = Engine::open(config).await.unwrap();
        engine
            .set_settings(Settings {
                torrent,
                ..Settings::default()
            })
            .await
            .unwrap();
        Rig {
            _tmp: tmp,
            dest,
            engine,
        }
    }

    /// Adds the seeder's magnet link with only `path` selected; answers the
    /// job and the indices of `a.bin` and `dir/b.bin`.
    async fn add_only(&self, seeder: &TorrentSeeder, path: &str) -> (JobId, usize, usize) {
        let info = self.engine.torrent_files(&seeder.magnet).await.unwrap();
        let index = |p: &str| info.files.iter().find(|f| f.path == p).unwrap().index;
        let id = self
            .engine
            .add(AddRequest {
                url: seeder.magnet.clone(),
                dest_dir: self.dest.clone(),
                only_files: Some(vec![index(path)]),
                ..AddRequest::default()
            })
            .await
            .unwrap();
        (id, index("a.bin"), index("dir/b.bin"))
    }

    fn view(&self, id: JobId) -> JobView {
        self.engine
            .snapshot()
            .into_iter()
            .find(|v| v.id == id)
            .expect("job in snapshot")
    }

    async fn until(&self, id: JobId, what: &str, done: impl Fn(&JobView) -> bool) {
        let deadline = Instant::now() + TIMEOUT;
        while !done(&self.view(id)) {
            assert!(
                Instant::now() < deadline,
                "never {what}: {:?}",
                self.view(id)
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

fn policy(stop_ratio: Option<f64>) -> TorrentSettings {
    TorrentSettings {
        stop_ratio,
        stop_after_secs: None,
        ..TorrentSettings::default()
    }
}

fn selection(view: &JobView) -> (u32, u32) {
    let torrent = view.torrent.as_ref().expect("a torrent view");
    (torrent.files_selected, torrent.files_total)
}

fn hash_and_mtime(path: &Path) -> (Vec<u8>, SystemTime) {
    let bytes = fs::read(path).unwrap();
    let modified = fs::metadata(path).unwrap().modified().unwrap();
    (Sha256::digest(&bytes).to_vec(), modified)
}

async fn wait_job(engine: &Engine, id: JobId) -> JobState {
    tokio::time::timeout(TIMEOUT, engine.wait(id))
        .await
        .expect("job did not settle in 30 s")
}

#[tokio::test(flavor = "multi_thread")]
async fn reselect_keeps_done_files_and_checks_space() {
    let (seeder, a, b) = seeder().await;
    let both = (a.len() + b.len()) as u64;
    // Shares for ever: the torrent stays in the session, sharing file 0.
    let rig = Rig::new(vec![seeder.addr], free_space, policy(None)).await;
    let (id, ia, ib) = rig.add_only(&seeder, "a.bin").await;
    rig.until(id, "seeding a.bin", |v| v.state == JobState::Seeding)
        .await;
    let a_path = rig.dest.join("pack/a.bin");
    let before = hash_and_mtime(&a_path);
    assert_eq!(fs::read(&a_path).unwrap(), a);
    // As the session counts it: whole pieces.
    let total = rig.view(id).total;

    // b.bin does not fit: refused, nothing changes.
    FREE.store(SPACE_MARGIN + 1024, Ordering::SeqCst);
    let e = rig
        .engine
        .set_torrent_files(id, vec![ib, ia])
        .await
        .unwrap_err();
    assert_eq!(e.kind, ErrorKind::DiskFull, "{}", e.detail);
    let view = rig.view(id);
    assert_eq!(view.state, JobState::Seeding);
    assert_eq!(view.total, total);
    assert_eq!(selection(&view), (1, 2));

    // With room, b.bin is downloaded beside a.bin, which is left alone.
    FREE.store(u64::MAX, Ordering::SeqCst);
    rig.engine
        .set_torrent_files(id, vec![ib, ia, ia])
        .await
        .unwrap();
    assert_eq!(selection(&rig.view(id)), (2, 2));
    assert_eq!(rig.view(id).total, Some(both));
    rig.until(id, "seeding both files", |v| {
        v.state == JobState::Seeding && v.total == Some(both) && v.downloaded == both
    })
    .await;
    assert_eq!(fs::read(rig.dest.join("pack/dir/b.bin")).unwrap(), b);
    assert_eq!(hash_and_mtime(&a_path), before, "a.bin was touched");

    // Its own rule stops the sharing: the job completes with both files.
    let rules = TorrentOverrides {
        stop_ratio: Some(Some(0.0)),
        stop_after_secs: None,
    };
    rig.engine
        .set_torrent_overrides(id, rules.clone())
        .await
        .unwrap();
    assert_eq!(wait_job(&rig.engine, id).await, JobState::Completed);
    let view = rig.view(id);
    assert_eq!((view.total, view.downloaded), (Some(both), both));
    assert_eq!(view.torrent.unwrap().overrides, rules);
    assert_eq!(hash_and_mtime(&a_path), before);
    rig.engine.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn completed_torrent_with_new_files_is_queued_again() {
    let (seeder, a, b) = seeder().await;
    let rig = Rig::new(vec![seeder.addr], free_space_plenty, policy(Some(0.0))).await;
    let (id, ia, ib) = rig.add_only(&seeder, "a.bin").await;
    assert_eq!(wait_job(&rig.engine, id).await, JobState::Completed);
    let a_path = rig.dest.join("pack/a.bin");
    let before = hash_and_mtime(&a_path).0;

    rig.engine
        .set_torrent_files(id, vec![ia, ib])
        .await
        .unwrap();
    assert_ne!(rig.view(id).state, JobState::Completed);
    assert_eq!(wait_job(&rig.engine, id).await, JobState::Completed);
    let view = rig.view(id);
    let both = (a.len() + b.len()) as u64;
    assert_eq!((view.total, view.downloaded), (Some(both), both));
    let b_path = rig.dest.join("pack/dir/b.bin");
    assert_eq!(fs::read(&b_path).unwrap(), b);
    assert_eq!(hash_and_mtime(&a_path).0, before);

    // Fewer files: still Completed, smaller, and what was left out stays.
    rig.engine.set_torrent_files(id, vec![ia]).await.unwrap();
    let view = rig.view(id);
    assert_eq!(view.state, JobState::Completed);
    assert_eq!(
        (view.total, view.downloaded),
        (Some(a.len() as u64), a.len() as u64)
    );
    assert_eq!(selection(&view), (1, 2));
    assert_eq!(fs::read(&b_path).unwrap(), b);
    rig.engine.shutdown().await.unwrap();
}

fn free_space_plenty(_: &Path) -> std::io::Result<u64> {
    Ok(u64::MAX)
}

#[tokio::test(flavor = "multi_thread")]
async fn empty_selection_rejected() {
    let (seeder, a, _) = seeder().await;
    let rig = Rig::new(vec![seeder.addr], free_space_plenty, policy(Some(0.0))).await;
    let (id, ia, _) = rig.add_only(&seeder, "a.bin").await;
    assert_eq!(wait_job(&rig.engine, id).await, JobState::Completed);

    let e = rig.engine.set_torrent_files(id, vec![]).await.unwrap_err();
    assert_eq!(e.kind, ErrorKind::Unknown);
    assert_eq!(e.detail, "select at least one file");
    let e = rig
        .engine
        .set_torrent_files(id, vec![ia, 2])
        .await
        .unwrap_err();
    assert_eq!(e.kind, ErrorKind::Unknown);
    assert!(e.detail.contains("no file 2"), "{}", e.detail);
    let view = rig.view(id);
    assert_eq!(view.state, JobState::Completed);
    assert_eq!(view.total, Some(a.len() as u64));
    assert_eq!(selection(&view), (1, 2));
    rig.engine.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn override_ratio_stops_earlier() {
    let (seeder, _, _) = seeder().await;
    // Globally, sharing never stops.
    let rig = Rig::new(vec![seeder.addr], free_space_plenty, policy(None)).await;
    let (id, _, _) = rig.add_only(&seeder, "a.bin").await;
    let rules = TorrentOverrides {
        stop_ratio: Some(Some(0.0)),
        stop_after_secs: None,
    };
    rig.engine
        .set_torrent_overrides(id, rules.clone())
        .await
        .unwrap();
    assert_eq!(rig.view(id).torrent.unwrap().overrides, rules);
    assert_eq!(wait_job(&rig.engine, id).await, JobState::Completed);
    assert_eq!(rig.view(id).torrent.unwrap().overrides, rules);

    // Only torrents have sharing rules.
    let e = rig
        .engine
        .set_torrent_overrides(JobId(id.0 + 100), TorrentOverrides::default())
        .await
        .unwrap_err();
    assert_eq!(e.kind, ErrorKind::Unknown);
    rig.engine.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn reselect_mid_download_goes_on() {
    // About five seconds for a.bin at 64 KiB/s.
    let a = data(300 * 1024, 41);
    let b = data(60 * 1024, 42);
    let files = [("a.bin", a.clone()), ("dir/b.bin", b.clone())];
    let seeder = TorrentSeeder::start("pack", &files, Some(64 * 1024)).await;
    let rig = Rig::new(vec![seeder.addr], free_space_plenty, policy(Some(0.0))).await;
    let (id, ia, ib) = rig.add_only(&seeder, "a.bin").await;
    rig.until(id, "partly downloaded", |v| {
        v.state == JobState::Downloading && v.downloaded >= 64 * 1024
    })
    .await;
    let before = rig.view(id).downloaded;
    assert!(before < a.len() as u64, "finished before the change");

    let engine = rig.engine.clone();
    let waiting = tokio::spawn(async move { engine.wait(id).await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    rig.engine
        .set_torrent_files(id, vec![ia, ib])
        .await
        .unwrap();
    // It goes on from what it has.
    let view = rig.view(id);
    assert_eq!(view.state, JobState::Downloading);
    assert!(view.downloaded >= before, "{} < {before}", view.downloaded);
    assert_eq!(wait_job(&rig.engine, id).await, JobState::Completed);
    // Waiting for the job is waiting through the change.
    assert_eq!(waiting.await.unwrap(), JobState::Completed);
    assert_eq!(fs::read(rig.dest.join("pack/a.bin")).unwrap(), a);
    assert_eq!(fs::read(rig.dest.join("pack/dir/b.bin")).unwrap(), b);
    // Nothing was downloaded twice, give or take the pieces in flight.
    let sent = seeder.uploaded_bytes();
    let both = (a.len() + b.len()) as u64;
    assert!(sent < both + 128 * 1024, "the seeder sent {sent} of {both}");
    rig.engine.shutdown().await.unwrap();
}
