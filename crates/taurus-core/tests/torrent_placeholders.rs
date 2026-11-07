//! The empty files librqbit makes for a torrent's unselected files are
//! deleted again, against real local librqbit sessions on 127.0.0.1.

use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use taurus_core::store::JobId;
use taurus_core::torrent::hub::TorrentTestNet;
use taurus_core::{
    AddRequest, Engine, EngineConfig, JobState, JobView, PowerMode, Settings, Timing,
    TorrentOverrides, TorrentSettings,
};
use taurus_testkit::{FileSpec, TorrentSeeder, content};
use tempfile::TempDir;

const TIMEOUT: Duration = Duration::from_secs(30);

fn data(size: usize, seed: u64) -> Vec<u8> {
    content(&FileSpec {
        seed,
        ..FileSpec::new(size)
    })
}

/// The seeder's three files, in whole pieces each (so an unselected file
/// gets no byte of a selected one's pieces).
const ONE: &str = "one/a.bin";
const TWO: &str = "b.bin";
const THREE: &str = "three/deep/c.bin";

/// Seeds `pack/` with [`ONE`], [`TWO`] and [`THREE`]; answers the seeder
/// and [`TWO`]'s content.
async fn seeder() -> (TorrentSeeder, Vec<u8>) {
    let two = data(48 * 1024, 62);
    let files = [
        (ONE, data(32 * 1024, 61)),
        (TWO, two.clone()),
        (THREE, data(64 * 1024, 63)),
    ];
    (TorrentSeeder::start("pack", &files, None).await, two)
}

struct Rig {
    tmp: TempDir,
    dest: PathBuf,
    engine: Engine,
}

impl Rig {
    async fn new(peers: Vec<SocketAddr>, autostart: bool, stop_ratio: Option<f64>) -> Rig {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("dest");
        fs::create_dir_all(&dest).unwrap();
        let mut config = EngineConfig::new(tmp.path().join("data"));
        config.timing = Timing::fast_for_tests();
        config.power = PowerMode::DryRun;
        config.autostart = autostart;
        config.torrent_test = Some(TorrentTestNet {
            initial_peers: peers,
            restore_in_place: false,
        });
        let engine = Engine::open(config).await.unwrap();
        let torrent = TorrentSettings {
            stop_ratio,
            stop_after_secs: None,
            ..TorrentSettings::default()
        };
        engine
            .set_settings(Settings {
                torrent,
                ..Settings::default()
            })
            .await
            .unwrap();
        Rig { tmp, dest, engine }
    }

    /// Adds `url` with only [`TWO`] selected. The index of a file follows
    /// the seeder's folder listing: it is looked up by path.
    async fn add_two(&self, url: &str) -> JobId {
        let info = self.engine.torrent_files(url).await.unwrap();
        let two = info.files.iter().find(|f| f.path == TWO).unwrap().index;
        self.engine
            .add(AddRequest {
                url: url.to_string(),
                dest_dir: self.dest.clone(),
                only_files: Some(vec![two]),
                ..AddRequest::default()
            })
            .await
            .unwrap()
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

    async fn wait(&self, id: JobId) -> JobState {
        tokio::time::timeout(TIMEOUT, self.engine.wait(id))
            .await
            .expect("job did not settle in 30 s")
    }
}

/// Where `path` (`/`-separated, inside the torrent) of the torrent `pack`
/// is saved in `dest`.
fn saved(dest: &Path, path: &str) -> PathBuf {
    path.split('/').fold(dest.join("pack"), |p, c| p.join(c))
}

#[tokio::test(flavor = "multi_thread")]
async fn unselected_files_leave_no_placeholders() {
    let (seeder, two) = seeder().await;
    // Shares for ever, until its own rule says stop.
    let rig = Rig::new(vec![seeder.addr], true, None).await;
    let id = rig.add_two(&seeder.magnet).await;
    rig.until(id, "seeding", |v| v.state == JobState::Seeding)
        .await;
    let pack = rig.dest.join("pack");
    // Sharing: librqbit still has the torrent, its unselected files gone.
    assert_eq!(fs::read(saved(&rig.dest, TWO)).unwrap(), two);
    assert!(!saved(&rig.dest, ONE).exists());
    assert!(!saved(&rig.dest, THREE).exists());
    assert!(!pack.join("one").exists());
    assert!(!pack.join("three").exists());

    let rules = TorrentOverrides {
        stop_ratio: Some(Some(0.0)),
        stop_after_secs: None,
    };
    rig.engine.set_torrent_overrides(id, rules).await.unwrap();
    assert_eq!(rig.wait(id).await, JobState::Completed);
    assert_eq!(fs::read(saved(&rig.dest, TWO)).unwrap(), two);
    assert!(!saved(&rig.dest, ONE).exists());
    assert!(!saved(&rig.dest, THREE).exists());
    // Only the folders they leave empty go.
    let mut left: Vec<String> = fs::read_dir(&pack)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    left.sort();
    assert_eq!(left, [TWO]);
    rig.engine.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn existing_unselected_file_is_kept() {
    let (seeder, two) = seeder().await;
    // Nothing runs until started: the file is made between the add (which
    // places the torrent in `dest/pack`) and the run.
    let rig = Rig::new(vec![seeder.addr], false, Some(0.0)).await;
    let file = rig.tmp.path().join("x.torrent");
    fs::write(&file, &seeder.torrent_bytes).unwrap();
    let id = rig.add_two(file.to_str().unwrap()).await;
    assert_eq!(rig.view(id).file_name, "pack");
    let one = saved(&rig.dest, ONE);
    fs::create_dir_all(one.parent().unwrap()).unwrap();
    fs::write(&one, b"").unwrap();

    rig.engine.start_now(id).await.unwrap();
    assert_eq!(rig.wait(id).await, JobState::Completed);
    assert_eq!(fs::read(saved(&rig.dest, TWO)).unwrap(), two);
    assert_eq!(fs::metadata(&one).unwrap().len(), 0, "the user's file went");
    assert!(!saved(&rig.dest, THREE).exists());
    assert!(!rig.dest.join("pack").join("three").exists());
    rig.engine.shutdown().await.unwrap();
}
