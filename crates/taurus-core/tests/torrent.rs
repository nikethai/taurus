//! TorrentHub against real local librqbit sessions on 127.0.0.1.

use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use taurus_core::store::{JobKind, NewJob, Store};
use taurus_core::torrent::TorrentFile;
use taurus_core::torrent::hub::{TorrentHub, TorrentTestNet};
use taurus_core::torrent::source::{Source, classify};
use taurus_core::{ErrorKind, JobState, TorrentSettings};
use taurus_testkit::{FileSpec, TorrentSeeder, content, make_torrent_bytes};

const TIMEOUT: Duration = Duration::from_secs(20);

fn data(size: usize, seed: u64) -> Vec<u8> {
    content(&FileSpec {
        seed,
        ..FileSpec::new(size)
    })
}

fn hub(data_dir: &Path, peers: Vec<std::net::SocketAddr>) -> TorrentHub {
    TorrentHub::new(
        data_dir,
        &TorrentSettings::default(),
        Some(TorrentTestNet {
            initial_peers: peers,
        }),
    )
}

async fn seeder() -> (TorrentSeeder, Vec<u8>, Vec<u8>) {
    let a = data(300 * 1024, 1);
    let b = data(100 * 1024, 2);
    let seeder = TorrentSeeder::start(
        "pack",
        &[("a.bin", a.clone()), ("dir/b.bin", b.clone())],
        None,
    )
    .await;
    (seeder, a, b)
}

async fn wait_finished(hub: &TorrentHub, info_hash: &str) {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let handle = hub.handle(info_hash).expect("torrent in session");
        if handle.stats().finished {
            return;
        }
        assert!(Instant::now() < deadline, "download did not finish");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn session_starts_lazily() {
    let tmp = tempfile::tempdir().unwrap();
    let hub = hub(tmp.path(), vec![]);
    let folder = tmp.path().join("torrent-session");
    hub.set_limits(Some(100_000), None);
    assert!(!folder.exists());
    assert_eq!(hub.listen_addr(), None);
    assert!(hub.handle(&"0".repeat(40)).is_none());

    let session = hub.session().await.unwrap();
    assert!(folder.is_dir());
    assert!(Arc::ptr_eq(&session, &hub.session().await.unwrap()));
    let addr = hub.listen_addr().expect("listens");
    assert!(addr.ip().is_loopback(), "{addr}");
    assert_ne!(addr.port(), 0);
    // Limits set before the start apply to the new session.
    assert_eq!(
        session.ratelimits.get_download_bps().map(|v| v.get()),
        Some(100_000)
    );
    hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn list_files_from_magnet() {
    let (seeder, a, b) = seeder().await;
    let tmp = tempfile::tempdir().unwrap();
    let hub = hub(tmp.path(), vec![seeder.addr]);
    let source = classify(&seeder.magnet).unwrap();
    let (info, bytes) = hub.list_files(&source, TIMEOUT).await.unwrap();

    assert_eq!(info.info_hash, seeder.info_hash);
    assert_eq!(info.name, "pack");
    assert_eq!(info.total, (a.len() + b.len()) as u64);
    let mut files = info.files.clone();
    files.sort_by(|x, y| x.path.cmp(&y.path));
    let index_of = |path: &str| info.files.iter().find(|f| f.path == path).unwrap().index;
    assert_eq!(
        files,
        [
            TorrentFile {
                index: index_of("a.bin"),
                path: "a.bin".into(),
                size: a.len() as u64
            },
            TorrentFile {
                index: index_of("dir/b.bin"),
                path: "dir/b.bin".into(),
                size: b.len() as u64
            },
        ]
    );
    let meta = librqbit::torrent_from_bytes(&bytes).unwrap();
    assert_eq!(meta.info_hash.as_string(), seeder.info_hash);
    // Listing does not add the torrent.
    assert!(hub.handle(&seeder.info_hash).is_none());
    hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn list_files_times_out_with_no_peers() {
    // A peer that accepts connections and never answers.
    let silent = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let hash = "1f".repeat(20);
    let source = classify(&format!("magnet:?xt=urn:btih:{hash}")).unwrap();

    let hub_silent = hub(&tmp.path().join("a"), vec![silent.local_addr().unwrap()]);
    hub_silent.session().await.unwrap();
    let started = Instant::now();
    let e = hub_silent
        .list_files(&source, Duration::from_secs(1))
        .await
        .unwrap_err();
    assert_eq!(e.kind, ErrorKind::NoPeers, "{}", e.detail);
    assert!(e.detail.contains("within 1 s"), "{}", e.detail);
    assert!(started.elapsed() < Duration::from_secs(5));
    hub_silent.shutdown().await;

    // With no peer source at all librqbit gives up at once: also NoPeers.
    let hub_none = hub(&tmp.path().join("b"), vec![]);
    hub_none.session().await.unwrap();
    let started = Instant::now();
    let e = hub_none
        .list_files(&source, Duration::from_secs(1))
        .await
        .unwrap_err();
    assert_eq!(e.kind, ErrorKind::NoPeers, "{}", e.detail);
    assert!(started.elapsed() < Duration::from_secs(1));
    hub_none.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn add_bytes_downloads_to_output() {
    let (seeder, a, b) = seeder().await;
    let tmp = tempfile::tempdir().unwrap();
    let out = tmp.path().join("out");
    let hub = hub(&tmp.path().join("data"), vec![seeder.addr]);
    let handle = hub
        .add_bytes(&seeder.torrent_bytes, &out, None, false)
        .await
        .unwrap();
    assert_eq!(handle.info_hash().as_string(), seeder.info_hash);
    wait_finished(&hub, &seeder.info_hash).await;

    // Straight into the output folder, no `pack` subfolder.
    assert_eq!(fs::read(out.join("a.bin")).unwrap(), a);
    assert_eq!(fs::read(out.join("dir").join("b.bin")).unwrap(), b);

    hub.pause(&seeder.info_hash).await.unwrap();
    hub.pause(&seeder.info_hash).await.unwrap();
    assert!(hub.handle(&seeder.info_hash).unwrap().is_paused());
    hub.unpause(&seeder.info_hash).await.unwrap();
    assert!(!hub.handle(&seeder.info_hash).unwrap().is_paused());
    hub.delete(&seeder.info_hash, true).await.unwrap();
    assert!(hub.handle(&seeder.info_hash).is_none());
    assert!(!out.join("a.bin").exists());
    hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn add_magnet_source() {
    let (seeder, a, _) = seeder().await;
    let tmp = tempfile::tempdir().unwrap();
    let out = tmp.path().join("out");
    let hub = hub(&tmp.path().join("data"), vec![seeder.addr]);
    let source = classify(&seeder.magnet).unwrap();
    assert!(matches!(source, Source::Magnet { .. }));
    hub.add(&source, &out, None, false).await.unwrap();
    wait_finished(&hub, &seeder.info_hash).await;
    assert_eq!(fs::read(out.join("a.bin")).unwrap(), a);
    hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn unsafe_torrent_is_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let hub = hub(&tmp.path().join("data"), vec![]);
    let bytes = make_torrent_bytes("x", &[("../evil.txt", b"boo")], 16_384);
    let out = tmp.path().join("out");
    let Err(e) = hub.add_bytes(&bytes, &out, None, true).await else {
        panic!("unsafe torrent added");
    };
    assert_eq!(e.kind, ErrorKind::UnsafeTorrent, "{}", e.detail);
    assert!(!tmp.path().join("evil.txt").exists());
    hub.shutdown().await;
}

fn torrent_job(dest: &Path, file_name: &str, meta: &[u8]) -> NewJob {
    let info_hash = librqbit::torrent_from_bytes(meta)
        .unwrap()
        .info_hash
        .as_string();
    NewJob {
        url: format!("magnet:?xt=urn:btih:{info_hash}"),
        source_url: format!("magnet:?xt=urn:btih:{info_hash}"),
        dest_dir: dest.to_path_buf(),
        file_name: file_name.to_string(),
        connections: 1,
        mirrors: vec![],
        checksum: None,
        referer: None,
        queue_pos: 0,
        kind: JobKind::Torrent,
        info_hash: Some(info_hash),
        torrent_meta: Some(meta.to_vec()),
        only_files: None,
    }
}

fn hash_of(meta: &[u8]) -> String {
    librqbit::torrent_from_bytes(meta)
        .unwrap()
        .info_hash
        .as_string()
}

#[tokio::test(flavor = "multi_thread")]
async fn reconcile_drops_unknown_and_restores_missing() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = tmp.path().join("data");
    let dest = tmp.path().join("out");
    fs::create_dir_all(&data_dir).unwrap();
    fs::create_dir_all(&dest).unwrap();
    let meta = |name: &str, seed: u64| {
        let (x, y) = (data(20_000, seed), data(5_000, seed + 1));
        make_torrent_bytes(name, &[("x.bin", &x), ("y.bin", &y)], 16_384)
    };
    let unknown = meta("unknown", 1);
    let completed = meta("completed", 3);
    let paused = meta("paused", 5);
    let missing = meta("missing", 7);
    let store = Store::open(&data_dir.join("taurus.db")).unwrap();
    let completed_id = store
        .insert_job(&torrent_job(&dest, "completed", &completed))
        .unwrap();
    store.set_state(completed_id, JobState::Completed).unwrap();
    let paused_id = store
        .insert_job(&torrent_job(&dest, "paused", &paused))
        .unwrap();
    store.set_state(paused_id, JobState::Paused).unwrap();
    store
        .insert_job(&torrent_job(&dest, "missing (1)", &missing))
        .unwrap();
    // A job whose metadata is broken is skipped, not fatal.
    let mut broken = torrent_job(&dest, "broken", &missing);
    broken.info_hash = Some("ab".repeat(20));
    broken.torrent_meta = Some(b"not a torrent".to_vec());
    store.insert_job(&broken).unwrap();

    // A first session that librqbit persists: the unknown, completed and
    // paused torrents, all running.
    let first = hub(&data_dir, vec![]);
    for (bytes, name) in [
        (&unknown, "unknown"),
        (&completed, "completed"),
        (&paused, "paused"),
    ] {
        first
            .add_bytes(bytes, &dest.join(name), None, false)
            .await
            .unwrap();
    }
    first.shutdown().await;

    // The next session restores them, then reconciles with the store.
    let second = hub(&data_dir, vec![]);
    let session = second.session().await.unwrap();
    assert_eq!(session.with_torrents(|it| it.count()), 3);
    second.reconcile(&store).await.unwrap();

    assert!(second.handle(&hash_of(&unknown)).is_none());
    assert!(second.handle(&hash_of(&completed)).is_none());
    // Not deleted with its files.
    assert!(dest.join("completed").join("x.bin").exists());
    let kept = second.handle(&hash_of(&paused)).expect("paused job kept");
    assert!(kept.is_paused());
    assert_eq!(kept.output_folder(), dest.join("paused"));
    let restored = second.handle(&hash_of(&missing)).expect("missing restored");
    assert!(restored.is_paused());
    assert_eq!(restored.output_folder(), dest.join("missing (1)"));
    assert_eq!(session.with_torrents(|it| it.count()), 2);

    // Reconciling again changes nothing.
    second.reconcile(&store).await.unwrap();
    assert_eq!(session.with_torrents(|it| it.count()), 2);
    second.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn limits_are_applied() {
    let tmp = tempfile::tempdir().unwrap();
    let hub = hub(tmp.path(), vec![]);
    let session = hub.session().await.unwrap();
    let get = || {
        (
            session.ratelimits.get_download_bps().map(|v| v.get()),
            session.ratelimits.get_upload_bps().map(|v| v.get()),
        )
    };
    assert_eq!(get(), (None, None));
    hub.set_limits(Some(100_000), Some(50_000));
    assert_eq!(get(), (Some(100_000), Some(50_000)));
    // Below librqbit's 16 KiB block the transfer would stall.
    hub.set_limits(Some(1), Some(0));
    assert_eq!(get(), (Some(16_384), Some(16_384)));
    hub.set_limits(Some(u64::MAX), None);
    assert_eq!(get(), (Some(u32::MAX), None));
    hub.shutdown().await;
}
