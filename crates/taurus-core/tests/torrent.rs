//! TorrentHub and the torrent job runner against real local librqbit
//! sessions on 127.0.0.1.

use std::collections::BTreeMap;
use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use librqbit::api::TorrentIdOrHash;
use librqbit::dht::Id20;
use librqbit::{Session, SessionOptions, SessionPersistenceConfig};
use taurus_core::engine::JobMsg;
use taurus_core::http::JobOutcome;
use taurus_core::store::{JobId, JobKind, NewJob, Store};
use taurus_core::system::space::SPACE_MARGIN;
use taurus_core::system::{SpaceSource, available_space};
use taurus_core::torrent::TorrentFile;
use taurus_core::torrent::hub::{TorrentHub, TorrentTestNet};
use taurus_core::torrent::job::{TorrentJobContext, run_torrent_job};
use taurus_core::torrent::source::{Source, classify};
use taurus_core::{
    AddRequest, Engine, EngineConfig, ErrorKind, Event, JobState, JobView, LinkInfo, PowerMode,
    RemoveFiles, Settings, Timing, TorrentSettings, WhenDone,
};
use taurus_testkit::{
    FileSpec, TestServer, TorrentLeecher, TorrentSeeder, content, make_torrent_bytes,
    test_session_options,
};
use tempfile::TempDir;
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;

const TIMEOUT: Duration = Duration::from_secs(30);

fn data(size: usize, seed: u64) -> Vec<u8> {
    content(&FileSpec {
        seed,
        ..FileSpec::new(size)
    })
}

fn hub(data_dir: &Path, peers: Vec<std::net::SocketAddr>) -> TorrentHub {
    fs::create_dir_all(data_dir).unwrap();
    let store = Store::open(&data_dir.join("taurus.db")).unwrap();
    hub_with(data_dir, peers, store)
}

fn hub_with(data_dir: &Path, peers: Vec<std::net::SocketAddr>, store: Store) -> TorrentHub {
    TorrentHub::new(
        data_dir,
        &TorrentSettings::default(),
        Some(TorrentTestNet {
            initial_peers: peers,
            restore_in_place: false,
        }),
        store,
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
    assert!(e.detail.contains("within 1s"), "{}", e.detail);
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
async fn unpause_while_files_are_checked() {
    let (seeder, a, _) = seeder().await;
    let tmp = tempfile::tempdir().unwrap();
    let out = tmp.path().join("out");
    let hub = hub(&tmp.path().join("data"), vec![seeder.addr]);
    // Still checking its files: librqbit would drop a plain start.
    hub.add_bytes(&seeder.torrent_bytes, &out, None, true)
        .await
        .unwrap();
    hub.unpause(&seeder.info_hash).await.unwrap();
    wait_finished(&hub, &seeder.info_hash).await;
    assert_eq!(fs::read(out.join("a.bin")).unwrap(), a);
    hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn unpause_after_a_pause_during_the_check() {
    // All its data is already on disk, so checking it takes a while.
    let big = data(32 * 1024 * 1024, 5);
    let meta = make_torrent_bytes("big", &[("big.bin", &big)], 256 * 1024);
    let tmp = tempfile::tempdir().unwrap();
    let out = tmp.path().join("out");
    fs::create_dir_all(&out).unwrap();
    fs::write(out.join("big.bin"), &big).unwrap();
    let hash = hash_of(&meta);
    let hub = hub(&tmp.path().join("data"), vec![]);
    hub.add_bytes(&meta, &out, None, false).await.unwrap();
    hub.pause(&hash).await.unwrap();
    let handle = hub.handle(&hash).unwrap();
    assert!(
        matches!(
            handle.stats().state,
            librqbit::TorrentStatsState::Initializing { .. }
        ),
        "paused during the check"
    );
    drop(handle);
    // The pause stops the check.
    tokio::time::sleep(Duration::from_millis(300)).await;

    tokio::time::timeout(TIMEOUT, hub.unpause(&hash))
        .await
        .expect("unpause returned")
        .unwrap();
    wait_finished(&hub, &hash).await;
    assert!(hub.handle(&hash).unwrap().live().is_some());
    hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_adds_get_their_own_torrents() {
    let tmp = tempfile::tempdir().unwrap();
    let out = tmp.path().join("out");
    let hub = hub(&tmp.path().join("data"), vec![]);
    let metas: Vec<Vec<u8>> = (0..8)
        .map(|n| make_torrent_bytes(&format!("t{n}"), &[("x.bin", &data(1000, n))], 16_384))
        .collect();
    let (out_x, out_y) = (out.join("x"), out.join("y"));
    for pair in metas.chunks(2) {
        let (x, y) = tokio::join!(
            hub.add_bytes(&pair[0], &out_x, None, true),
            hub.add_bytes(&pair[1], &out_y, None, true),
        );
        assert_eq!(x.unwrap().info_hash().as_string(), hash_of(&pair[0]));
        assert_eq!(y.unwrap().info_hash().as_string(), hash_of(&pair[1]));
    }
    let session = hub.session().await.unwrap();
    assert_eq!(session.with_torrents(|it| it.count()), metas.len());
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
    // So is one whose stored name is not a plain name.
    let bad_name = meta("bad", 9);
    store
        .insert_job(&torrent_job(&dest, "../escape", &bad_name))
        .unwrap();

    // A first session, with no jobs, that librqbit persists: the unknown,
    // completed and paused torrents, all running.
    let empty = Store::open(&tmp.path().join("empty.db")).unwrap();
    let first = hub_with(&data_dir, vec![], empty);
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
    drop(first);

    // The next session restores them and, as it starts, reconciles with the
    // store.
    let second = hub_with(&data_dir, vec![], store.clone());
    let session = second.session().await.unwrap();

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
    assert!(second.handle(&hash_of(&bad_name)).is_none());
    assert!(!tmp.path().join("escape").exists());
    assert_eq!(session.with_torrents(|it| it.count()), 2);

    // Reconciling again changes nothing.
    second.reconcile().await.unwrap();
    assert_eq!(session.with_torrents(|it| it.count()), 2);
    second.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn any_start_reconciles() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = tmp.path().join("data");
    let dest = tmp.path().join("out");
    let stale = make_torrent_bytes("stale", &[("s.bin", &data(20_000, 1))], 16_384);
    let other = make_torrent_bytes("other", &[("o.bin", &data(20_000, 2))], 16_384);
    let first = hub(&data_dir, vec![]);
    first
        .add_bytes(&stale, &dest.join("stale"), None, false)
        .await
        .unwrap();
    first.shutdown().await;
    drop(first);

    // No job owns `stale`: the start an unrelated add causes drops it.
    let second = hub(&data_dir, vec![]);
    second
        .add_bytes(&other, &dest.join("other"), None, true)
        .await
        .unwrap();
    assert!(second.handle(&hash_of(&stale)).is_none());
    let session = second.session().await.unwrap();
    assert_eq!(session.with_torrents(|it| it.count()), 1);
    second.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn shutdown_pauses_saves_and_releases() {
    let (seeder, _, _) = seeder().await;
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = tmp.path().join("data");
    let out = tmp.path().join("out");
    let hub = hub(&data_dir, vec![seeder.addr]);
    hub.add_bytes(&seeder.torrent_bytes, &out, None, false)
        .await
        .unwrap();
    wait_finished(&hub, &seeder.info_hash).await;
    let weak = Arc::downgrade(&hub.session().await.unwrap());

    hub.shutdown().await;
    // The session, and with it every open file, is let go.
    let deadline = Instant::now() + Duration::from_secs(5);
    while weak.upgrade().is_some() {
        assert!(Instant::now() < deadline, "session still held");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    fs::remove_dir_all(&out).unwrap();
    let Err(e) = hub.session().await else {
        panic!("the session started again after shutdown");
    };
    assert!(e.detail.contains("shut down"), "{}", e.detail);
    assert!(hub.handle(&seeder.info_hash).is_none());
    assert_eq!(hub.listen_addr(), None);
    drop(hub);

    // librqbit saved the torrent as paused, so a plain restore of its
    // session keeps it paused.
    let folder = data_dir.join("torrent-session");
    let opts = SessionOptions {
        persistence: Some(SessionPersistenceConfig::Json {
            folder: Some(folder.clone()),
        }),
        ..test_session_options(&folder)
    };
    let raw = Session::new_with_opts(folder, opts).await.unwrap();
    let id = Id20::from_str(&seeder.info_hash).unwrap();
    let restored = raw.get(TorrentIdOrHash::Hash(id)).expect("persisted");
    assert!(restored.is_paused());
    raw.cancellation_token().cancel();

    // And a new hub on the same data dir starts.
    let again = hub_with(
        &data_dir,
        vec![],
        Store::open(&data_dir.join("taurus.db")).unwrap(),
    );
    again.session().await.unwrap();
    again.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn torrent_url_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let hub = hub(tmp.path(), vec![]);
    let source = classify("https://example.org/x.torrent").unwrap();
    let e = hub
        .list_files(&source, Duration::from_secs(1))
        .await
        .unwrap_err();
    assert!(e.detail.contains("fetch"), "{}", e.detail);
    let Err(e) = hub.add(&source, tmp.path(), None, true).await else {
        panic!("a .torrent URL was added");
    };
    assert!(e.detail.contains("fetch"), "{}", e.detail);
    hub.shutdown().await;
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

// The job runner.

/// A store and a hub in a temp dir, and the folder jobs save into.
struct Rig {
    tmp: TempDir,
    dest: PathBuf,
    store: Store,
    hub: Arc<TorrentHub>,
}

fn rig(peers: Vec<SocketAddr>) -> Rig {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = tmp.path().join("data");
    let dest = tmp.path().join("dest");
    fs::create_dir_all(&data_dir).unwrap();
    fs::create_dir_all(&dest).unwrap();
    let store = Store::open(&data_dir.join("taurus.db")).unwrap();
    let hub = Arc::new(hub_with(&data_dir, peers, store.clone()));
    Rig {
        tmp,
        dest,
        store,
        hub,
    }
}

/// A torrent job as the engine adds it: from a magnet link, from a local
/// `.torrent` file (`source` is its path) with no metadata yet, or with its
/// metadata already stored and placed (`meta`).
fn job_for(dest: &Path, source: &str, name: &str, meta: Option<&[u8]>) -> NewJob {
    NewJob {
        url: source.to_string(),
        source_url: source.to_string(),
        dest_dir: dest.to_path_buf(),
        file_name: name.to_string(),
        connections: 1,
        mirrors: vec![],
        checksum: None,
        referer: None,
        queue_pos: 0,
        kind: JobKind::Torrent,
        info_hash: meta.map(hash_of),
        torrent_meta: meta.map(<[u8]>::to_vec),
        only_files: None,
    }
}

fn policy(stop_ratio: Option<f64>, stop_after_secs: Option<u64>) -> TorrentSettings {
    TorrentSettings {
        stop_ratio,
        stop_after_secs,
        ..TorrentSettings::default()
    }
}

fn context(
    rig: &Rig,
    id: JobId,
    settings: TorrentSettings,
    space: SpaceSource,
) -> (
    TorrentJobContext,
    mpsc::UnboundedReceiver<JobMsg>,
    CancellationToken,
) {
    let (msgs, rx) = mpsc::unbounded_channel();
    let cancel = CancellationToken::new();
    let ctx = TorrentJobContext {
        id,
        hub: rig.hub.clone(),
        store: rig.store.clone(),
        timing: Timing::fast_for_tests(),
        settings: tokio::sync::watch::channel(settings).1,
        space,
        reserved: Arc::new(|_| 0),
        cancel: cancel.clone(),
        msgs,
    };
    (ctx, rx, cancel)
}

async fn run(ctx: TorrentJobContext) -> JobOutcome {
    tokio::time::timeout(Duration::from_secs(30), run_torrent_job(ctx))
        .await
        .expect("the runner ended")
}

/// Everything sent until the runner dropped its sender.
async fn drain(rx: &mut mpsc::UnboundedReceiver<JobMsg>) -> Vec<JobMsg> {
    let mut all = Vec::new();
    while let Some(msg) = rx.recv().await {
        all.push(msg);
    }
    all
}

fn failed(outcome: JobOutcome) -> taurus_core::CoreError {
    match outcome {
        JobOutcome::Failed(e) => e,
        other => panic!("expected a failure, got {other:?}"),
    }
}

/// A seeder sending at 64 KiB/s, so a download takes a few seconds.
async fn slow_seeder(sizes: (usize, usize)) -> (TorrentSeeder, Vec<u8>, Vec<u8>) {
    let a = data(sizes.0, 11);
    let b = data(sizes.1, 12);
    let seeder = TorrentSeeder::start(
        "pack",
        &[("a.bin", a.clone()), ("dir/b.bin", b.clone())],
        Some(64 * 1024),
    )
    .await;
    (seeder, a, b)
}

fn one_kib_free(_: &Path) -> std::io::Result<u64> {
    Ok(1024)
}

#[tokio::test(flavor = "multi_thread")]
async fn runner_downloads_and_seeds_until_ratio() {
    let (seeder, a, b) = seeder().await;
    let rig = rig(vec![seeder.addr]);
    // A magnet job: the runner fetches the metadata and places the job.
    let mut new = job_for(&rig.dest, &seeder.magnet, "pack", None);
    new.info_hash = Some(seeder.info_hash.clone());
    let id = rig.store.insert_job(&new).unwrap();
    let (ctx, mut rx, _cancel) = context(&rig, id, policy(Some(0.0), None), available_space);

    let JobOutcome::Completed(out) = run(ctx).await else {
        panic!("not completed");
    };
    // Stopped as soon as it finished: it never turned Seeding.
    let msgs = drain(&mut rx).await;
    assert!(!msgs.iter().any(|m| matches!(m, JobMsg::Seeding { .. })));
    assert!(msgs.iter().any(|m| matches!(m, JobMsg::Progress { .. })));
    assert_eq!(out, rig.dest.join("pack"));
    assert_eq!(fs::read(out.join("a.bin")).unwrap(), a);
    assert_eq!(fs::read(out.join("dir").join("b.bin")).unwrap(), b);

    let job = rig.store.get_job(id).unwrap().unwrap();
    assert_eq!(job.state, JobState::Completed);
    assert_eq!(job.file_name, "pack");
    assert_eq!(job.info_hash.as_deref(), Some(seeder.info_hash.as_str()));
    assert_eq!(
        hash_of(job.torrent_meta.as_deref().unwrap()),
        seeder.info_hash
    );
    assert_eq!(job.total, Some((a.len() + b.len()) as u64));
    assert!(job.seed_started_at.is_some());
    // Sharing stopped: the torrent left the session, its files kept.
    assert!(rig.hub.handle(&seeder.info_hash).is_none());
    rig.hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn runner_places_a_magnet_beside_a_taken_name() {
    let (seeder, a, _) = seeder().await;
    let rig = rig(vec![seeder.addr]);
    fs::create_dir(rig.dest.join("pack")).unwrap();
    fs::write(rig.dest.join("pack").join("a.bin"), b"neighbour").unwrap();
    let id = rig
        .store
        .insert_job(&job_for(&rig.dest, &seeder.magnet, "pack", None))
        .unwrap();
    let (ctx, _rx, _cancel) = context(&rig, id, policy(Some(0.0), None), available_space);

    let JobOutcome::Completed(out) = run(ctx).await else {
        panic!("not completed");
    };
    assert_eq!(out, rig.dest.join("pack (1)"));
    assert_eq!(fs::read(out.join("a.bin")).unwrap(), a);
    assert_eq!(
        fs::read(rig.dest.join("pack").join("a.bin")).unwrap(),
        b"neighbour"
    );
    assert_eq!(
        rig.store.get_job(id).unwrap().unwrap().file_name,
        "pack (1)"
    );
    rig.hub.shutdown().await;
}

// Four workers: the two runners block on the placement lock meanwhile.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn runner_placement_is_serialized() {
    let rig = rig(vec![]);
    // Two torrents with the same name, from `.torrent` files not read yet:
    // each runner reads its file and places its job.
    let mut runs = Vec::new();
    for n in 0..2 {
        let a = data(20_000, 30 + n);
        let meta = make_torrent_bytes("pack", &[("a.bin", &a), ("b.bin", b"b")], 16_384);
        let file = rig.tmp.path().join(format!("{n}.torrent"));
        fs::write(&file, &meta).unwrap();
        let new = job_for(&rig.dest, file.to_str().unwrap(), "pack", None);
        let id = rig.store.insert_job(&new).unwrap();
        let (ctx, _, cancel) = context(&rig, id, policy(Some(0.0), None), available_space);
        runs.push((id, cancel, ctx));
    }

    // While the lock is held, neither job records a folder.
    let held = rig.hub.placement_lock();
    let mut tasks = Vec::new();
    for (id, cancel, ctx) in runs {
        tasks.push((id, cancel, tokio::spawn(run(ctx))));
    }
    // Blocks only the test's own thread, not the runtime's workers.
    std::thread::sleep(Duration::from_secs(1));
    for (id, _, _) in &tasks {
        assert!(
            rig.store
                .get_job(*id)
                .unwrap()
                .unwrap()
                .torrent_meta
                .is_none()
        );
    }
    drop(held);

    // Nobody seeds them: once both are placed, stop them.
    let deadline = Instant::now() + TIMEOUT;
    let placed = || {
        tasks.iter().all(|(id, _, _)| {
            let hash = rig.store.get_job(*id).unwrap().unwrap().info_hash;
            hash.is_some_and(|h| rig.hub.handle(&h).is_some())
        })
    };
    while !placed() {
        assert!(Instant::now() < deadline, "jobs not placed");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let mut names = Vec::new();
    for (id, cancel, task) in tasks {
        cancel.cancel();
        assert!(matches!(task.await.unwrap(), JobOutcome::Paused));
        let job = rig.store.get_job(id).unwrap().unwrap();
        let handle = rig.hub.handle(job.info_hash.as_deref().unwrap()).unwrap();
        assert_eq!(handle.output_folder(), rig.dest.join(&job.file_name));
        names.push(job.file_name);
    }
    names.sort();
    assert_eq!(names, ["pack", "pack (1)"]);
    rig.hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn runner_reports_progress_and_seeding_phase() {
    let (seeder, a, b) = slow_seeder((160 * 1024, 40 * 1024)).await;
    let total = (a.len() + b.len()) as u64;
    let rig = rig(vec![seeder.addr]);
    let new = job_for(
        &rig.dest,
        &seeder.magnet,
        "pack",
        Some(&seeder.torrent_bytes),
    );
    let id = rig.store.insert_job(&new).unwrap();
    // Nobody downloads from this job, so the time limit ends the seeding.
    let (ctx, mut rx, _cancel) = context(&rig, id, policy(Some(1.0), Some(1)), available_space);

    let runner = tokio::spawn(run(ctx));
    let msgs = drain(&mut rx).await;
    let JobOutcome::Completed(out) = runner.await.unwrap() else {
        panic!("not completed");
    };
    assert_eq!(fs::read(out.join("a.bin")).unwrap(), a);

    let seeding: Vec<usize> = msgs
        .iter()
        .enumerate()
        .filter(|(_, m)| matches!(m, JobMsg::Seeding { id: s, finished_now: true } if *s == id))
        .map(|(i, _)| i)
        .collect();
    assert_eq!(seeding.len(), 1, "one Seeding message");
    let partial = msgs[..seeding[0]].iter().any(
        |m| matches!(m, JobMsg::Progress { downloaded, total: Some(t), .. } if *downloaded < *t),
    );
    assert!(partial, "progress before the download finished");
    for m in &msgs {
        if let JobMsg::Progress {
            id: p,
            downloaded,
            total: t,
            ..
        } = m
        {
            assert_eq!(*p, id);
            assert_eq!(*t, Some(total));
            assert!(*downloaded <= total);
        }
    }
    let done = msgs[seeding[0]..]
        .iter()
        .any(|m| matches!(m, JobMsg::Progress { downloaded, .. } if *downloaded == total));
    assert!(done, "complete progress while seeding");
    let view = msgs
        .iter()
        .find_map(|m| match m {
            JobMsg::TorrentStats { id: s, view } if *s == id => Some(view.clone()),
            _ => None,
        })
        .expect("torrent stats");
    assert_eq!((view.files_selected, view.files_total), (2, 2));

    let job = rig.store.get_job(id).unwrap().unwrap();
    assert_eq!(job.state, JobState::Completed);
    assert!(job.seed_started_at.is_some());
    rig.hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn runner_rejects_unsafe_torrent() {
    let rig = rig(vec![]);
    // librqbit itself refuses `..`: read from a `.torrent` file.
    let evil = make_torrent_bytes(
        "evil",
        &[("../evil.txt", b"boo"), ("ok.txt", b"ok")],
        16_384,
    );
    let file = rig.tmp.path().join("evil.torrent");
    fs::write(&file, &evil).unwrap();
    let id = rig
        .store
        .insert_job(&job_for(&rig.dest, file.to_str().unwrap(), "evil", None))
        .unwrap();
    let (ctx, _rx, _cancel) = context(&rig, id, policy(Some(0.0), None), available_space);
    let e = failed(run(ctx).await);
    assert_eq!(e.kind, ErrorKind::UnsafeTorrent, "{}", e.detail);
    let job = rig.store.get_job(id).unwrap().unwrap();
    assert_eq!(job.state, JobState::Failed);
    assert_eq!(job.error.map(|e| e.0), Some(ErrorKind::UnsafeTorrent));
    assert!(job.torrent_meta.is_none());

    // A name librqbit would write but Taurus refuses, from stored metadata.
    let odd = make_torrent_bytes("odd", &[("a\u{1}b.txt", b"boo"), ("ok.txt", b"ok")], 16_384);
    let id = rig
        .store
        .insert_job(&job_for(&rig.dest, "odd.torrent", "odd", Some(&odd)))
        .unwrap();
    let (ctx, _rx, _cancel) = context(&rig, id, policy(Some(0.0), None), available_space);
    let e = failed(run(ctx).await);
    assert_eq!(e.kind, ErrorKind::UnsafeTorrent, "{}", e.detail);
    assert!(rig.hub.handle(&hash_of(&odd)).is_none());

    assert!(!rig.tmp.path().join("evil.txt").exists());
    assert_eq!(fs::read_dir(&rig.dest).unwrap().count(), 0);
    rig.hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn runner_rejects_duplicate_torrent() {
    let (seeder, _, _) = seeder().await;
    let rig = rig(vec![seeder.addr]);
    let first = job_for(
        &rig.dest,
        &seeder.magnet,
        "pack",
        Some(&seeder.torrent_bytes),
    );
    let first = rig.store.insert_job(&first).unwrap();
    rig.store.set_state(first, JobState::Paused).unwrap();
    let id = rig
        .store
        .insert_job(&job_for(&rig.dest, &seeder.magnet, "pack (1)", None))
        .unwrap();
    let (ctx, _rx, _cancel) = context(&rig, id, policy(Some(0.0), None), available_space);

    let e = failed(run(ctx).await);
    assert_eq!(e.kind, ErrorKind::AlreadyAdded);
    assert!(
        e.detail.contains(&format!("job {}", first.0)),
        "{}",
        e.detail
    );
    let job = rig.store.get_job(id).unwrap().unwrap();
    assert_eq!((job.info_hash, job.torrent_meta), (None, None));
    rig.hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn runner_space_check() {
    let (seeder, _, _) = seeder().await;
    let rig = rig(vec![seeder.addr]);
    let new = job_for(
        &rig.dest,
        &seeder.magnet,
        "pack",
        Some(&seeder.torrent_bytes),
    );
    let id = rig.store.insert_job(&new).unwrap();
    let (ctx, _rx, _cancel) = context(&rig, id, policy(Some(0.0), None), one_kib_free);

    let e = failed(run(ctx).await);
    assert_eq!(e.kind, ErrorKind::DiskFull, "{}", e.detail);
    // Refused before the torrent was added: nothing was written.
    assert!(rig.hub.handle(&seeder.info_hash).is_none());
    assert!(!rig.dest.join("pack").exists());
    let job = rig.store.get_job(id).unwrap().unwrap();
    assert_eq!(job.state, JobState::Failed);
    assert_eq!(job.error.map(|e| e.0), Some(ErrorKind::DiskFull));
    rig.hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn runner_cancel_pauses() {
    // About ten seconds at 64 KiB/s: still downloading when cancelled.
    let (seeder, _, _) = slow_seeder((500 * 1024, 100 * 1024)).await;
    let rig = rig(vec![seeder.addr]);
    let new = job_for(
        &rig.dest,
        &seeder.magnet,
        "pack",
        Some(&seeder.torrent_bytes),
    );
    let id = rig.store.insert_job(&new).unwrap();
    // Uploaded in earlier runs.
    rig.store.commit_torrent_progress(id, 0, 1_000).unwrap();
    let (ctx, mut rx, cancel) = context(&rig, id, policy(None, None), available_space);
    let runner = tokio::spawn(run(ctx));

    // Another peer downloads from this job, so it uploads. It joins once
    // the job has pieces: librqbit tells a peer that has nothing about its
    // pieces only in the bitfield it sends as they connect.
    let started = Instant::now();
    let mut leecher = None;
    let mut uploaded = 0;
    while started.elapsed() < Duration::from_secs(1) || uploaded <= 1_000 {
        assert!(started.elapsed() < TIMEOUT, "nothing uploaded");
        let msg = tokio::time::timeout(TIMEOUT, rx.recv())
            .await
            .unwrap()
            .unwrap();
        match msg {
            JobMsg::Progress { downloaded, .. } if downloaded >= 32 * 1024 && leecher.is_none() => {
                let addr = rig.hub.listen_addr().unwrap();
                leecher = Some(TorrentLeecher::start(&seeder.torrent_bytes, addr).await);
            }
            JobMsg::TorrentStats { view, .. } => uploaded = view.uploaded,
            _ => {}
        }
    }
    cancel.cancel();
    let rest = drain(&mut rx).await;
    assert!(matches!(runner.await.unwrap(), JobOutcome::Paused));
    let last = rest
        .iter()
        .rev()
        .find_map(|m| match m {
            JobMsg::TorrentStats { view, .. } => Some(view.uploaded),
            _ => None,
        })
        .unwrap_or(uploaded);

    let job = rig.store.get_job(id).unwrap().unwrap();
    assert_eq!(job.state, JobState::Paused);
    assert_eq!(job.uploaded, last);
    assert!(job.uploaded > 1_000);
    assert!(job.seed_started_at.is_none());
    assert!(rig.hub.handle(&seeder.info_hash).unwrap().is_paused());
    drop(leecher);
    rig.hub.shutdown().await;
}

// The engine.

/// An engine whose torrents reach `peers`, saving into `<tmp>/dest`.
struct EngineRig {
    tmp: TempDir,
    dest: PathBuf,
    engine: Engine,
}

impl EngineRig {
    async fn new(peers: Vec<SocketAddr>, torrent: TorrentSettings) -> EngineRig {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("dest");
        fs::create_dir_all(&dest).unwrap();
        let engine = Engine::open(engine_config(tmp.path(), peers, available_space))
            .await
            .unwrap();
        engine
            .set_settings(Settings {
                torrent,
                ..Settings::default()
            })
            .await
            .unwrap();
        EngineRig { tmp, dest, engine }
    }

    fn request(&self, input: &str) -> AddRequest {
        AddRequest {
            url: input.to_string(),
            dest_dir: self.dest.clone(),
            ..AddRequest::default()
        }
    }

    fn view(&self, id: JobId) -> JobView {
        self.engine
            .snapshot()
            .into_iter()
            .find(|v| v.id == id)
            .expect("job in snapshot")
    }
}

fn engine_config(dir: &Path, peers: Vec<SocketAddr>, space: SpaceSource) -> EngineConfig {
    let mut config = EngineConfig::new(dir.join("data"));
    config.timing = Timing::fast_for_tests();
    config.power = PowerMode::DryRun;
    config.available_space = space;
    config.torrent_test = Some(TorrentTestNet {
        initial_peers: peers,
        restore_in_place: false,
    });
    config
}

async fn wait_job(engine: &Engine, id: JobId) -> JobState {
    tokio::time::timeout(Duration::from_secs(30), engine.wait(id))
        .await
        .expect("job did not settle in 30 s")
}

/// Events until (and including) the first one `stop` accepts, within 30 s
/// (a sharing torrent sends progress for ever).
async fn events_until(
    events: &mut broadcast::Receiver<Event>,
    stop: impl Fn(&Event) -> bool,
) -> Vec<Event> {
    let mut seen = Vec::new();
    let all = async {
        loop {
            let event = events.recv().await.expect("event channel open");
            let done = stop(&event);
            seen.push(event);
            if done {
                return;
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(30), all)
        .await
        .expect("the awaited event did not come in 30 s");
    seen
}

fn updated_to(event: &Event, id: JobId, state: JobState) -> bool {
    matches!(event, Event::JobUpdated(v) if v.id == id && v.state == state)
}

async fn until_state(rig: &EngineRig, id: JobId, state: JobState) {
    let deadline = Instant::now() + TIMEOUT;
    while rig.view(id).state != state {
        assert!(Instant::now() < deadline, "job never got {state:?}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn add_magnet_downloads() {
    let (seeder, a, b) = seeder().await;
    let rig = EngineRig::new(vec![seeder.addr], policy(Some(0.0), None)).await;
    let mut events = rig.engine.subscribe();
    let id = rig.engine.add(rig.request(&seeder.magnet)).await.unwrap();
    assert_eq!(wait_job(&rig.engine, id).await, JobState::Completed);

    let seen = events_until(&mut events, |e| updated_to(e, id, JobState::Completed)).await;
    assert!(matches!(&seen[0], Event::JobAdded(v) if v.id == id && v.kind == JobKind::Torrent));
    assert!(
        seen.iter()
            .any(|e| matches!(e, Event::Progress { id: p, .. } if *p == id))
    );
    // Stopped as soon as it finished: never Seeding.
    assert!(!seen.iter().any(|e| updated_to(e, id, JobState::Seeding)));
    assert_eq!(fs::read(rig.dest.join("pack/a.bin")).unwrap(), a);
    assert_eq!(fs::read(rig.dest.join("pack/dir/b.bin")).unwrap(), b);
    let view = rig.view(id);
    assert_eq!(view.file_name, "pack");
    assert_eq!(view.total, Some((a.len() + b.len()) as u64));
    assert_eq!(view.downloaded, view.total.unwrap());
    let torrent = view.torrent.expect("a torrent view");
    assert_eq!((torrent.files_selected, torrent.files_total), (2, 2));
    rig.engine.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn seeding_job_pauses_and_resumes() {
    let (seeder, _, _) = seeder().await;
    // Never stops sharing on its own.
    let rig = EngineRig::new(vec![seeder.addr], policy(None, None)).await;
    let mut events = rig.engine.subscribe();
    let id = rig.engine.add(rig.request(&seeder.magnet)).await.unwrap();
    events_until(&mut events, |e| updated_to(e, id, JobState::Seeding)).await;
    assert_eq!(rig.view(id).state, JobState::Seeding);
    // Sharing is not busy: the queue is idle.
    events_until(&mut events, |e| matches!(e, Event::QueueIdle)).await;

    rig.engine.pause(id).await.unwrap();
    assert_eq!(rig.view(id).state, JobState::Paused);
    // Resuming queues it again, without asking any server.
    rig.engine.resume(id).await.unwrap();
    events_until(&mut events, |e| updated_to(e, id, JobState::Seeding)).await;
    rig.engine.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn remove_if_unfinished_keeps_complete_torrent_files() {
    let (seeder, a, _) = seeder().await;
    let rig = EngineRig::new(vec![seeder.addr], policy(None, None)).await;
    let mut events = rig.engine.subscribe();
    let id = rig.engine.add(rig.request(&seeder.magnet)).await.unwrap();
    events_until(&mut events, |e| updated_to(e, id, JobState::Seeding)).await;

    // Paused after it finished: all its data is there, the files are the
    // user's.
    rig.engine.pause(id).await.unwrap();
    assert_eq!(rig.view(id).state, JobState::Paused);
    rig.engine
        .remove_with(id, RemoveFiles::IfUnfinished)
        .await
        .unwrap();
    assert!(rig.engine.snapshot().is_empty());
    assert_eq!(fs::read(rig.dest.join("pack/a.bin")).unwrap(), a);
    rig.engine.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn add_torrent_file_and_url() {
    let (seeder, a, b) = seeder().await;
    let rig = EngineRig::new(vec![seeder.addr], policy(Some(0.0), None)).await;
    let file = rig.tmp.path().join("x.torrent");
    fs::write(&file, &seeder.torrent_bytes).unwrap();
    let id = rig
        .engine
        .add(rig.request(file.to_str().unwrap()))
        .await
        .unwrap();
    // Placed at once: the metadata is known.
    assert_eq!(rig.view(id).file_name, "pack");
    assert_eq!(wait_job(&rig.engine, id).await, JobState::Completed);
    assert_eq!(fs::read(rig.dest.join("pack/a.bin")).unwrap(), a);

    // Removed with its files, the same torrent can come again by URL.
    rig.engine.remove(id, true).await.unwrap();
    assert!(!rig.dest.join("pack").exists());
    let server = TestServer::start().await;
    let url = server.serve_bytes(
        "/x.torrent",
        "application/x-bittorrent",
        &seeder.torrent_bytes,
    );
    let id = rig.engine.add(rig.request(&url)).await.unwrap();
    assert_eq!(wait_job(&rig.engine, id).await, JobState::Completed);
    assert_eq!(fs::read(rig.dest.join("pack/dir/b.bin")).unwrap(), b);
    rig.engine.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn only_selected_files_written() {
    let (seeder, a, b) = seeder().await;
    let rig = EngineRig::new(vec![seeder.addr], policy(Some(0.0), None)).await;
    let info = rig.engine.torrent_files(&seeder.magnet).await.unwrap();
    let index = info.files.iter().find(|f| f.path == "a.bin").unwrap().index;
    let id = rig
        .engine
        .add(AddRequest {
            only_files: Some(vec![index]),
            ..rig.request(&seeder.magnet)
        })
        .await
        .unwrap();
    assert_eq!(wait_job(&rig.engine, id).await, JobState::Completed);
    assert_eq!(fs::read(rig.dest.join("pack/a.bin")).unwrap(), a);
    let other = fs::read(rig.dest.join("pack/dir/b.bin")).unwrap_or_default();
    assert_ne!(other, b, "an unselected file was downloaded");
    let view = rig.view(id);
    assert_eq!(view.total, Some(a.len() as u64));
    assert_eq!(
        view.torrent.map(|t| (t.files_selected, t.files_total)),
        Some((1, 2))
    );

    // An index the torrent does not have is refused at once.
    let file = rig.tmp.path().join("x.torrent");
    fs::write(&file, &seeder.torrent_bytes).unwrap();
    rig.engine.remove(id, false).await.unwrap();
    let e = rig
        .engine
        .add(AddRequest {
            only_files: Some(vec![2]),
            ..rig.request(file.to_str().unwrap())
        })
        .await
        .unwrap_err();
    assert_eq!(e.kind, ErrorKind::Unknown, "{}", e.detail);
    assert!(rig.engine.snapshot().is_empty());
    rig.engine.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn same_magnet_twice_is_already_added() {
    let (seeder, _, _) = seeder().await;
    let tmp = tempfile::tempdir().unwrap();
    let mut config = engine_config(tmp.path(), vec![seeder.addr], available_space);
    // Nothing runs: only the adds are under test.
    config.autostart = false;
    let engine = Engine::open(config).await.unwrap();
    let dest = tmp.path().to_path_buf();
    let request = |url: &str| AddRequest {
        url: url.to_string(),
        dest_dir: dest.clone(),
        ..AddRequest::default()
    };
    let id = engine.add(request(&seeder.magnet)).await.unwrap();
    let e = engine.add(request(&seeder.magnet)).await.unwrap_err();
    assert_eq!(e.kind, ErrorKind::AlreadyAdded, "{}", e.detail);
    assert!(e.detail.contains(&format!("job {}", id.0)), "{}", e.detail);
    // The same torrent from its file is the same torrent.
    let file = tmp.path().join("x.torrent");
    fs::write(&file, &seeder.torrent_bytes).unwrap();
    let e = engine
        .add(request(file.to_str().unwrap()))
        .await
        .unwrap_err();
    assert_eq!(e.kind, ErrorKind::AlreadyAdded, "{}", e.detail);
    assert_eq!(engine.snapshot().len(), 1);
    engine.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn inspect_torrent() {
    let (seeder, a, b) = seeder().await;
    let rig = EngineRig::new(vec![seeder.addr], TorrentSettings::default()).await;
    let LinkInfo::Torrent(info) = rig.engine.inspect(&seeder.magnet).await.unwrap() else {
        panic!("a magnet link is a torrent");
    };
    assert_eq!(info.info_hash, seeder.info_hash);
    assert_eq!(info.name, "pack");
    assert_eq!(info.total, (a.len() + b.len()) as u64);
    let file = rig.tmp.path().join("x.torrent");
    fs::write(&file, &seeder.torrent_bytes).unwrap();
    assert_eq!(
        rig.engine.inspect(file.to_str().unwrap()).await.unwrap(),
        LinkInfo::Torrent(info)
    );
    // Nothing was added.
    assert!(rig.engine.snapshot().is_empty());
    assert!(fs::read_dir(&rig.dest).unwrap().next().is_none());
    let store = Store::open(&rig.tmp.path().join("data").join("taurus.db")).unwrap();
    assert!(store.list_jobs().unwrap().is_empty());
    rig.engine.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn find_by_url_matches_info_hash() {
    let (seeder, _, _) = seeder().await;
    let rig = EngineRig::new(vec![seeder.addr], TorrentSettings::default()).await;
    let id = rig.engine.add(rig.request(&seeder.magnet)).await.unwrap();

    assert_eq!(rig.engine.find_by_url(&seeder.magnet), Some(id));
    // Another spelling of the same magnet link: upper case, no name.
    let other = format!("magnet:?xt=urn:btih:{}", seeder.info_hash.to_uppercase());
    assert_eq!(rig.engine.find_by_url(&other), Some(id));
    // The .torrent file of the same torrent.
    let file = rig.tmp.path().join("same.torrent");
    fs::write(&file, &seeder.torrent_bytes).unwrap();
    assert_eq!(rig.engine.find_by_url(file.to_str().unwrap()), Some(id));
    // Another torrent.
    let unknown = format!("magnet:?xt=urn:btih:{}", "1".repeat(40));
    assert_eq!(rig.engine.find_by_url(&unknown), None);
    let view = rig.view(id);
    assert_eq!(view.info_hash.as_deref(), Some(seeder.info_hash.as_str()));
    rig.engine.shutdown().await.unwrap();

    // The other way round: a job added from the .torrent file is found by
    // its magnet link.
    let rig = EngineRig::new(vec![seeder.addr], TorrentSettings::default()).await;
    let file = rig.tmp.path().join("added.torrent");
    fs::write(&file, &seeder.torrent_bytes).unwrap();
    let id = rig
        .engine
        .add(rig.request(file.to_str().unwrap()))
        .await
        .unwrap();
    assert_eq!(rig.engine.find_by_url(&seeder.magnet), Some(id));
    assert_eq!(rig.engine.find_by_url(&other), Some(id));
    rig.engine.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn torrent_files_lists_without_adding() {
    let (seeder, a, b) = seeder().await;
    let rig = EngineRig::new(vec![seeder.addr], TorrentSettings::default()).await;
    assert_eq!(rig.engine.torrent_listen_addr(), None);
    let info = rig.engine.torrent_files(&seeder.magnet).await.unwrap();
    assert_eq!(info.info_hash, seeder.info_hash);
    assert_eq!(info.name, "pack");
    assert_eq!(info.total, (a.len() + b.len()) as u64);
    assert_eq!(info.files.len(), 2);
    let file = rig.tmp.path().join("x.torrent");
    fs::write(&file, &seeder.torrent_bytes).unwrap();
    assert_eq!(
        rig.engine
            .torrent_files(file.to_str().unwrap())
            .await
            .unwrap(),
        info
    );
    assert!(rig.engine.snapshot().is_empty());
    // Listing a magnet needed the session.
    assert!(rig.engine.torrent_listen_addr().is_some());
    rig.engine.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn torrents_have_their_own_cap() {
    let (seeder, _, _) = slow_seeder((500 * 1024, 100 * 1024)).await;
    let server = TestServer::start().await;
    let mut spec = FileSpec::new(1024 * 1024);
    spec.bytes_per_sec = Some(64 * 1024);
    let url = server.serve("/big.bin", spec);
    let rig = EngineRig::new(
        vec![seeder.addr],
        TorrentSettings {
            max_active: 1,
            ..policy(Some(0.0), None)
        },
    )
    .await;
    let mut settings = rig.engine.settings();
    settings.max_active = 1;
    rig.engine.set_settings(settings).await.unwrap();

    let http = rig.engine.add(rig.request(&url)).await.unwrap();
    let torrent = rig.engine.add(rig.request(&seeder.magnet)).await.unwrap();
    until_state(&rig, torrent, JobState::Downloading).await;
    assert_eq!(rig.view(http).state, JobState::Downloading);
    // A second torrent waits for the torrent slot, not the HTTP one.
    let file = rig.tmp.path().join("other.torrent");
    fs::write(
        &file,
        make_torrent_bytes("other", &[("o.bin", &[7; 1000])], 16_384),
    )
    .unwrap();
    let second = rig
        .engine
        .add(rig.request(file.to_str().unwrap()))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(rig.view(second).state, JobState::Queued);
    rig.engine.shutdown().await.unwrap();
}

fn little_space(_: &Path) -> std::io::Result<u64> {
    Ok(SPACE_MARGIN + 1024 * 1024)
}

#[tokio::test(flavor = "multi_thread")]
async fn torrent_space_check_counts_running_downloads() {
    let (seeder, _, _) = seeder().await;
    let server = TestServer::start().await;
    let mut spec = FileSpec::new(900 * 1024);
    spec.bytes_per_sec = Some(32 * 1024);
    let url = server.serve("/big.bin", spec);
    let tmp = tempfile::tempdir().unwrap();
    let engine = Engine::open(engine_config(tmp.path(), vec![seeder.addr], little_space))
        .await
        .unwrap();
    let request = |url: &str| AddRequest {
        url: url.to_string(),
        dest_dir: tmp.path().to_path_buf(),
        ..AddRequest::default()
    };
    let http = engine.add(request(&url)).await.unwrap();
    // 400 KiB fits on its own, not beside the 900 KiB still downloading.
    let torrent = engine.add(request(&seeder.magnet)).await.unwrap();
    assert_eq!(wait_job(&engine, torrent).await, JobState::Failed);
    let view = engine
        .snapshot()
        .into_iter()
        .find(|v| v.id == torrent)
        .unwrap();
    assert_eq!(view.error.map(|e| e.0), Some(ErrorKind::DiskFull));
    engine.pause(http).await.unwrap();
    engine.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn torrent_requests_are_checked() {
    let rig = EngineRig::new(vec![], TorrentSettings::default()).await;
    let magnet = format!("magnet:?xt=urn:btih:{}", "1f".repeat(20));
    let e = rig
        .engine
        .add(AddRequest {
            mirrors: vec!["http://127.0.0.1:9/x".into()],
            ..rig.request(&magnet)
        })
        .await
        .unwrap_err();
    assert_eq!(
        (e.kind, e.detail.as_str()),
        (
            ErrorKind::Unknown,
            "mirrors and checksum apply only to web downloads"
        )
    );
    let e = rig
        .engine
        .add(AddRequest {
            only_files: Some(vec![0]),
            ..rig.request("http://127.0.0.1:9/x.bin")
        })
        .await
        .unwrap_err();
    assert_eq!(
        (e.kind, e.detail.as_str()),
        (
            ErrorKind::Unknown,
            "file selection applies only to torrents"
        )
    );
    let e = rig
        .engine
        .add(rig.request("magnet:?dn=nothing"))
        .await
        .unwrap_err();
    assert_eq!(e.kind, ErrorKind::SiteNotSupported);
    // A torrent max_active of 0 would queue torrents for ever.
    let mut settings = rig.engine.settings();
    settings.torrent.max_active = 0;
    let e = rig.engine.set_settings(settings).await.unwrap_err();
    assert_eq!(
        (e.kind, e.detail.as_str()),
        (ErrorKind::Unknown, "torrent.max_active must be at least 1")
    );
    assert!(rig.engine.snapshot().is_empty());
    // A local .torrent file is read no further than a fetched one.
    let big = rig.tmp.path().join("big.torrent");
    fs::write(&big, vec![b'd'; 10 * 1024 * 1024 + 1]).unwrap();
    for e in [
        rig.engine
            .torrent_files(big.to_str().unwrap())
            .await
            .unwrap_err(),
        rig.engine
            .add(rig.request(big.to_str().unwrap()))
            .await
            .unwrap_err(),
    ] {
        assert_eq!(e.detail, "the .torrent file is larger than 10 MiB");
    }
    // HTTP-only use never starts the torrent session.
    assert_eq!(rig.engine.torrent_listen_addr(), None);
    rig.engine.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn open_with_torrent_jobs_starts_the_session() {
    let (seeder, _, _) = seeder().await;
    let tmp = tempfile::tempdir().unwrap();
    let config = |autostart| {
        let mut config = engine_config(tmp.path(), vec![seeder.addr], available_space);
        config.autostart = autostart;
        config
    };
    let engine = Engine::open(config(false)).await.unwrap();
    let request = AddRequest {
        url: seeder.magnet.clone(),
        dest_dir: tmp.path().to_path_buf(),
        ..AddRequest::default()
    };
    let id = engine.add(request).await.unwrap();
    engine.shutdown().await.unwrap();

    // Only looking at the jobs: no session.
    let engine = Engine::open(config(false)).await.unwrap();
    assert_eq!(engine.torrent_listen_addr(), None);
    engine.shutdown().await.unwrap();
    // A queue that runs a torrent starts it at once.
    let engine = Engine::open(config(true)).await.unwrap();
    assert!(engine.torrent_listen_addr().is_some());
    engine.pause(id).await.unwrap();
    engine.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn seeding_fires_the_when_done_action() {
    let (seeder, _, _) = seeder().await;
    let rig = EngineRig::new(vec![seeder.addr], policy(None, None)).await;
    let mut settings = rig.engine.settings();
    settings.when_done = WhenDone::Notify;
    rig.engine.set_settings(settings).await.unwrap();
    let mut events = rig.engine.subscribe();
    let id = rig.engine.add(rig.request(&seeder.magnet)).await.unwrap();
    // Finishing the download empties the queue, even though it shares on.
    let seen = events_until(&mut events, |e| {
        matches!(
            e,
            Event::WhenDoneFired {
                action: WhenDone::Notify,
                ..
            }
        )
    })
    .await;
    assert!(seen.iter().any(|e| updated_to(e, id, JobState::Seeding)));
    assert_eq!(rig.view(id).state, JobState::Seeding);
    rig.engine.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn seeding_rules_apply_to_a_seeding_torrent() {
    let (seeder, _, _) = seeder().await;
    let rig = EngineRig::new(vec![seeder.addr], policy(None, None)).await;
    let mut events = rig.engine.subscribe();
    let id = rig.engine.add(rig.request(&seeder.magnet)).await.unwrap();
    events_until(&mut events, |e| updated_to(e, id, JobState::Seeding)).await;
    let mut settings = rig.engine.settings();
    settings.torrent.stop_after_secs = Some(1);
    rig.engine.set_settings(settings).await.unwrap();
    assert_eq!(wait_job(&rig.engine, id).await, JobState::Completed);
    rig.engine.shutdown().await.unwrap();
}

// End to end: the milestone's done criteria.

const MIB: usize = 1024 * 1024;

fn progress_of(event: &Event, id: JobId) -> Option<u64> {
    match event {
        Event::Progress {
            id: p, downloaded, ..
        } if *p == id => Some(*downloaded),
        _ => None,
    }
}

/// Every file under `dir`, by its `/`-joined path inside it.
fn tree(dir: &Path) -> BTreeMap<String, Vec<u8>> {
    fn walk(dir: &Path, prefix: &str, out: &mut BTreeMap<String, Vec<u8>>) {
        for entry in fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let name = format!("{prefix}{}", entry.file_name().to_string_lossy());
            if entry.file_type().unwrap().is_dir() {
                walk(&entry.path(), &format!("{name}/"), out);
            } else {
                out.insert(name, fs::read(entry.path()).unwrap());
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(dir, "", &mut out);
    out
}

fn files_of(files: &[(&str, Vec<u8>)]) -> BTreeMap<String, Vec<u8>> {
    files
        .iter()
        .map(|(path, data)| (path.to_string(), data.clone()))
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn restart_resumes_without_redownload() {
    let file = data(2 * MIB, 21);
    // About eight seconds for the whole file.
    let seeder = TorrentSeeder::start("big", &[("big.bin", file.clone())], Some(256 * 1024)).await;
    let rig = EngineRig::new(vec![seeder.addr], policy(Some(0.0), None)).await;
    let mut events = rig.engine.subscribe();
    let id = rig.engine.add(rig.request(&seeder.magnet)).await.unwrap();
    let seen = events_until(&mut events, |e| {
        progress_of(e, id).is_some_and(|d| d >= MIB as u64)
    })
    .await;
    let before = seen.iter().rev().find_map(|e| progress_of(e, id)).unwrap();
    assert!(before < file.len() as u64, "finished before the restart");
    let EngineRig { tmp, dest, engine } = rig;
    engine.shutdown().await.unwrap();
    drop(engine);

    // Interrupted, not paused: it comes back Queued.
    let store = Store::open(&tmp.path().join("data").join("taurus.db")).unwrap();
    assert_eq!(store.get_job(id).unwrap().unwrap().state, JobState::Queued);
    drop(store);

    let engine = Engine::open(engine_config(
        tmp.path(),
        vec![seeder.addr],
        available_space,
    ))
    .await
    .unwrap();
    let mut events = engine.subscribe();
    assert_eq!(wait_job(&engine, id).await, JobState::Completed);
    // It goes on from what is on disk.
    let mut first = None;
    while let Ok(event) = events.try_recv() {
        first = first.or(progress_of(&event, id));
    }
    let first = first.expect("progress after the restart");
    assert!(first >= before, "restarted at {first}, had {before}");
    let view = engine.snapshot().into_iter().find(|v| v.id == id).unwrap();
    assert_eq!(fs::read(dest.join(&view.file_name)).unwrap(), file);
    // Nothing was downloaded twice, give or take the pieces in flight.
    let uploaded = seeder.uploaded_bytes();
    assert!(
        uploaded < 3 * MIB as u64,
        "the seeder sent {uploaded} bytes"
    );
    engine.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn ratio_stop_after_leecher() {
    let (seeder, a, b) = seeder().await;
    let total = (a.len() + b.len()) as u64;
    let rig = EngineRig::new(vec![seeder.addr], policy(Some(1.0), None)).await;
    let mut events = rig.engine.subscribe();
    let id = rig.engine.add(rig.request(&seeder.magnet)).await.unwrap();
    events_until(&mut events, |e| updated_to(e, id, JobState::Seeding)).await;

    // Another peer downloads the whole torrent from the job alone.
    let addr = rig
        .engine
        .torrent_listen_addr()
        .expect("the session listens");
    let leecher = TorrentLeecher::start(&seeder.torrent_bytes, addr).await;
    assert!(leecher.wait_complete(TIMEOUT).await, "the leecher finished");
    let finished = Instant::now();
    let seen = events_until(&mut events, |e| updated_to(e, id, JobState::Completed)).await;
    assert!(
        finished.elapsed() < Duration::from_secs(10),
        "stopped sharing {:?} after the leecher finished",
        finished.elapsed()
    );
    let Some(Event::JobUpdated(done)) = seen.last() else {
        unreachable!()
    };
    let torrent = done.torrent.clone().expect("a torrent view");
    assert!(torrent.ratio >= 1.0, "ratio {}", torrent.ratio);
    assert!(torrent.uploaded >= total);
    // The view is kept as it completed.
    assert!(rig.view(id).torrent.unwrap().ratio >= 1.0);
    rig.engine.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn seed_time_stop() {
    let (seeder, _, _) = seeder().await;
    let rig = EngineRig::new(vec![seeder.addr], policy(None, Some(1))).await;
    let mut events = rig.engine.subscribe();
    let id = rig.engine.add(rig.request(&seeder.magnet)).await.unwrap();
    events_until(&mut events, |e| updated_to(e, id, JobState::Seeding)).await;
    let seeding = Instant::now();
    events_until(&mut events, |e| updated_to(e, id, JobState::Completed)).await;
    // The clock starts only as the Seeding event arrives, after the store
    // write and the actor: the lower bound guards against stopping at once,
    // not the exact time.
    let shared = seeding.elapsed();
    assert!(
        shared >= Duration::from_millis(600) && shared < Duration::from_secs(5),
        "shared for {shared:?}"
    );
    assert_eq!(rig.view(id).state, JobState::Completed);
    rig.engine.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn seeding_frees_slot_and_does_not_block_idle() {
    // A few seconds: the second torrent is added while it downloads.
    let (slow, _, _) = slow_seeder((200 * 1024, 50 * 1024)).await;
    let other = TorrentSeeder::start("other", &[("o.bin", data(100 * 1024, 13))], None).await;
    let rig = EngineRig::new(
        vec![slow.addr, other.addr],
        TorrentSettings {
            max_active: 1,
            ..policy(None, None)
        },
    )
    .await;
    let mut events = rig.engine.subscribe();
    let first = rig.engine.add(rig.request(&slow.magnet)).await.unwrap();
    let second = rig.engine.add(rig.request(&other.magnet)).await.unwrap();
    assert_eq!(rig.view(first).state, JobState::Downloading);
    assert_eq!(rig.view(second).state, JobState::Queued);

    let seen = events_until(&mut events, |e| updated_to(e, first, JobState::Seeding)).await;
    assert!(
        !seen
            .iter()
            .any(|e| updated_to(e, second, JobState::Downloading))
    );
    assert!(!seen.iter().any(|e| matches!(e, Event::QueueIdle)));
    // The sharing torrent left the slot: the second starts, and once it
    // shares too the queue is idle.
    let seen = events_until(&mut events, |e| matches!(e, Event::QueueIdle)).await;
    let started = seen
        .iter()
        .position(|e| updated_to(e, second, JobState::Downloading))
        .expect("the second torrent started");
    let shared = seen
        .iter()
        .position(|e| updated_to(e, second, JobState::Seeding))
        .expect("the second torrent shares");
    assert!(started < shared);
    assert_eq!(rig.view(first).state, JobState::Seeding);
    assert_eq!(rig.view(second).state, JobState::Seeding);
    rig.engine.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn magnet_without_peers_waits() {
    // A peer that accepts connections and never answers: like a magnet
    // nobody shares, the metadata never comes.
    let silent = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let rig = EngineRig::new(
        vec![silent.local_addr().unwrap()],
        TorrentSettings::default(),
    )
    .await;
    let timing = Timing::fast_for_tests();
    let mut events = rig.engine.subscribe();
    let magnet = format!("magnet:?xt=urn:btih:{}", "5e".repeat(20));
    let added = Instant::now();
    let id = rig.engine.add(rig.request(&magnet)).await.unwrap();

    for attempt in 0..2 {
        let seen = events_until(&mut events, |e| updated_to(e, id, JobState::Waiting)).await;
        if attempt == 0 {
            assert!(added.elapsed() >= timing.torrent_metadata_timeout);
        }
        let Some(Event::JobUpdated(view)) = seen.last() else {
            unreachable!()
        };
        let (kind, detail) = view.error.clone().expect("an error");
        assert_eq!(kind, ErrorKind::NoPeers, "{detail}");
        // Retried, not at once.
        let waiting = Instant::now();
        events_until(&mut events, |e| updated_to(e, id, JobState::Downloading)).await;
        assert!(waiting.elapsed() >= timing.waiting_retry_first / 2);
    }
    rig.engine.pause(id).await.unwrap();
    assert_eq!(rig.view(id).state, JobState::Paused);
    rig.engine.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn same_name_twice_gets_suffix_folder() {
    let one = [
        ("ep1.mkv", data(120 * 1024, 41)),
        ("ep2.mkv", data(80 * 1024, 42)),
    ];
    let two = [
        ("ep3.mkv", data(100 * 1024, 43)),
        ("extras/x.txt", data(1000, 44)),
    ];
    let first = TorrentSeeder::start("Show", &one, None).await;
    let second = TorrentSeeder::start("Show", &two, None).await;
    let rig = EngineRig::new(vec![first.addr, second.addr], policy(Some(0.0), None)).await;
    let a = rig.engine.add(rig.request(&first.magnet)).await.unwrap();
    let b = rig.engine.add(rig.request(&second.magnet)).await.unwrap();
    assert_eq!(wait_job(&rig.engine, a).await, JobState::Completed);
    assert_eq!(wait_job(&rig.engine, b).await, JobState::Completed);

    let mut folders = Vec::new();
    for (id, files) in [(a, &one), (b, &two)] {
        let name = rig.view(id).file_name;
        assert_eq!(tree(&rig.dest.join(&name)), files_of(files), "{name}");
        folders.push(name);
    }
    folders.sort();
    assert_eq!(folders, ["Show", "Show (1)"]);
    let mut entries: Vec<String> = fs::read_dir(&rig.dest)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    entries.sort();
    assert_eq!(entries, folders);
    rig.engine.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn remove_with_files_touches_only_its_files() {
    let (pack, _, _) = seeder().await;
    let one = data(200 * 1024, 5);
    let single = TorrentSeeder::start("one", &[("one.bin", one.clone())], None).await;
    let rig = EngineRig::new(vec![pack.addr, single.addr], policy(Some(0.0), None)).await;
    let add_and_finish = |input: String| {
        let engine = &rig.engine;
        let request = rig.request(&input);
        async move {
            let id = engine.add(request).await.unwrap();
            assert_eq!(wait_job(engine, id).await, JobState::Completed);
            id
        }
    };

    // A single file saved straight into an otherwise empty save folder:
    // the folder stays.
    let id = add_and_finish(single.magnet.clone()).await;
    let file = rig.view(id).file_name;
    assert_eq!(fs::read(rig.dest.join(&file)).unwrap(), one);
    rig.engine.remove(id, true).await.unwrap();
    assert!(!rig.dest.join(&file).exists());
    assert!(rig.dest.is_dir(), "the save folder was deleted");

    fs::write(rig.dest.join("keep.txt"), b"mine").unwrap();
    // A multi-file torrent: its files and its own folder go.
    let id = add_and_finish(pack.magnet.clone()).await;
    let folder = rig.dest.join(rig.view(id).file_name);
    assert!(folder.join("a.bin").is_file());
    rig.engine.remove(id, true).await.unwrap();
    assert!(!folder.exists(), "the torrent's folder is left");
    // A single file beside the neighbour.
    let id = add_and_finish(single.magnet.clone()).await;
    let file = rig.view(id).file_name;
    rig.engine.remove(id, true).await.unwrap();
    assert!(!rig.dest.join(&file).exists());

    assert_eq!(tree(&rig.dest), files_of(&[("keep.txt", b"mine".to_vec())]));
    assert!(rig.engine.snapshot().is_empty());
    rig.engine.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn pause_and_resume_torrent() {
    // About six seconds at 64 KiB/s.
    let (seeder, a, b) = slow_seeder((300 * 1024, 60 * 1024)).await;
    let total = (a.len() + b.len()) as u64;
    let rig = EngineRig::new(vec![seeder.addr], policy(Some(0.0), None)).await;
    let mut events = rig.engine.subscribe();
    let id = rig.engine.add(rig.request(&seeder.magnet)).await.unwrap();
    events_until(&mut events, |e| {
        progress_of(e, id).is_some_and(|d| d >= 64 * 1024)
    })
    .await;

    rig.engine.pause(id).await.unwrap();
    let paused = rig.view(id);
    assert_eq!(paused.state, JobState::Paused);
    assert!(paused.downloaded < total, "finished before the pause");
    while events.try_recv().is_ok() {}
    // A second with no progress.
    tokio::time::sleep(Duration::from_secs(1)).await;
    let mut moved = Vec::new();
    while let Ok(event) = events.try_recv() {
        moved.extend(progress_of(&event, id));
    }
    assert!(moved.is_empty(), "progress while paused: {moved:?}");
    assert_eq!(rig.view(id).downloaded, paused.downloaded);
    assert_eq!(rig.view(id).state, JobState::Paused);

    rig.engine.resume(id).await.unwrap();
    assert_eq!(wait_job(&rig.engine, id).await, JobState::Completed);
    let folder = rig.dest.join(rig.view(id).file_name);
    assert_eq!(tree(&folder), files_of(&[("a.bin", a), ("dir/b.bin", b)]));
    rig.engine.shutdown().await.unwrap();
}

// What a later session start must not bring back.

/// Engines on one data dir in `<tmp>/data`, saving into `<tmp>/dest`.
struct Restarts {
    tmp: TempDir,
    dest: PathBuf,
    peers: Vec<SocketAddr>,
    /// Restarts the way the product does: see `TorrentTestNet`.
    in_place: bool,
}

impl Restarts {
    fn new(peers: Vec<SocketAddr>) -> Restarts {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("dest");
        fs::create_dir_all(&dest).unwrap();
        Restarts {
            tmp,
            dest,
            peers,
            in_place: false,
        }
    }

    /// Without `autostart`, as a front end that only looks at the jobs: the
    /// torrent session does not start.
    async fn open(&self, autostart: bool, torrent: TorrentSettings) -> Engine {
        let mut config = engine_config(self.tmp.path(), self.peers.clone(), available_space);
        config.autostart = autostart;
        if let Some(test) = &mut config.torrent_test {
            test.restore_in_place = self.in_place;
        }
        let engine = Engine::open(config).await.unwrap();
        engine
            .set_settings(Settings {
                torrent,
                ..Settings::default()
            })
            .await
            .unwrap();
        engine
    }

    fn request(&self, input: &str) -> AddRequest {
        AddRequest {
            url: input.to_string(),
            dest_dir: self.dest.clone(),
            ..AddRequest::default()
        }
    }

    /// The ids librqbit saved its torrents under.
    fn session_ids(&self) -> Vec<String> {
        let list: serde_json::Value = serde_json::from_str(&self.session_file()).unwrap();
        let ids = list["torrents"].as_object().expect("a torrent list");
        ids.keys().cloned().collect()
    }

    fn session_file(&self) -> String {
        let path = self.tmp.path().join("data/torrent-session/session.json");
        fs::read_to_string(path).unwrap_or_default()
    }

    /// Queues `magnet` while the session is not running, then opens an
    /// engine that starts the session for it and lets it complete.
    async fn start_session_for(&self, magnet: &str) {
        let engine = self.open(false, policy(Some(0.0), None)).await;
        let id = engine.add(self.request(magnet)).await.unwrap();
        engine.shutdown().await.unwrap();
        drop(engine);
        let engine = self.open(true, policy(Some(0.0), None)).await;
        assert_eq!(wait_job(&engine, id).await, JobState::Completed);
        engine.shutdown().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn removed_torrent_does_not_come_back() {
    let (seeder, _, _) = seeder().await;
    let other = TorrentSeeder::start("other", &[("o.bin", data(100 * 1024, 13))], None).await;
    let rs = Restarts::new(vec![seeder.addr, other.addr]);
    // Sharing as Taurus stops: librqbit saves it.
    let engine = rs.open(true, policy(None, None)).await;
    let mut events = engine.subscribe();
    let id = engine.add(rs.request(&seeder.magnet)).await.unwrap();
    events_until(&mut events, |e| updated_to(e, id, JobState::Seeding)).await;
    engine.shutdown().await.unwrap();
    drop(engine);
    assert!(rs.session_file().contains(&seeder.info_hash));

    // Removed with its files while the session is not running.
    let engine = rs.open(false, policy(None, None)).await;
    engine.remove(id, true).await.unwrap();
    engine.shutdown().await.unwrap();
    drop(engine);
    assert!(!rs.dest.join("pack").exists());

    // A later session, started for another torrent, does not restore it.
    rs.start_session_for(&other.magnet).await;
    assert_eq!(
        sizes(&rs.dest),
        BTreeMap::from([("o.bin".into(), 100 * 1024)])
    );
    assert!(!rs.session_file().contains(&seeder.info_hash));
}

#[tokio::test(flavor = "multi_thread")]
async fn completed_torrent_files_moved_away_do_not_come_back() {
    let (seeder, a, b) = seeder().await;
    let other = TorrentSeeder::start("other", &[("o.bin", data(100 * 1024, 13))], None).await;
    let rs = Restarts::new(vec![seeder.addr, other.addr]);
    let engine = rs.open(true, policy(Some(0.0), None)).await;
    let id = engine.add(rs.request(&seeder.magnet)).await.unwrap();
    assert_eq!(wait_job(&engine, id).await, JobState::Completed);
    engine.shutdown().await.unwrap();
    drop(engine);

    // The user moves the finished files elsewhere.
    let moved = rs.tmp.path().join("moved");
    fs::rename(rs.dest.join("pack"), &moved).unwrap();
    rs.start_session_for(&other.magnet).await;
    assert_eq!(
        sizes(&rs.dest),
        BTreeMap::from([("o.bin".into(), 100 * 1024)])
    );
    assert!(tree(&moved) == files_of(&[("a.bin", a), ("dir/b.bin", b)]));
}

#[tokio::test(flavor = "multi_thread")]
async fn unreadable_session_list_does_not_block_the_start() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = tmp.path().join("data");
    let dest = tmp.path().join("out");
    let folder = data_dir.join("torrent-session");
    fs::create_dir_all(&folder).unwrap();
    fs::create_dir_all(&dest).unwrap();
    fs::write(folder.join("session.json"), b"{ not json").unwrap();
    let meta = make_torrent_bytes("kept", &[("k.bin", &data(20_000, 1))], 16_384);
    let store = Store::open(&data_dir.join("taurus.db")).unwrap();
    store
        .insert_job(&torrent_job(&dest, "kept", &meta))
        .unwrap();

    let hub = hub_with(&data_dir, vec![], store);
    hub.session().await.unwrap();
    // The store's torrent is added back.
    assert!(hub.handle(&hash_of(&meta)).is_some());
    hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn remove_with_files_deletes_empty_placeholders() {
    // Whole pieces per file: the unselected file gets no byte.
    let a = data(64 * 1024, 51);
    let seeder = TorrentSeeder::start(
        "pair",
        &[("a.bin", a.clone()), ("b.bin", data(32 * 1024, 52))],
        None,
    )
    .await;
    let rs = Restarts::new(vec![seeder.addr]);
    let engine = rs.open(true, policy(Some(0.0), None)).await;
    let info = engine.torrent_files(&seeder.magnet).await.unwrap();
    let index = info.files.iter().find(|f| f.path == "a.bin").unwrap().index;
    let request = AddRequest {
        only_files: Some(vec![index]),
        ..rs.request(&seeder.magnet)
    };
    let id = engine.add(request).await.unwrap();
    assert_eq!(wait_job(&engine, id).await, JobState::Completed);
    engine.shutdown().await.unwrap();
    drop(engine);
    let folder = rs.dest.join("pair");
    assert_eq!(fs::read(folder.join("a.bin")).unwrap(), a);
    assert_eq!(fs::metadata(folder.join("b.bin")).unwrap().len(), 0);

    // Removed while the session is not running: the files are ours to
    // delete, the empty one librqbit made too.
    let engine = rs.open(false, policy(Some(0.0), None)).await;
    engine.remove(id, true).await.unwrap();
    assert!(!folder.exists(), "left: {:?}", sizes(&rs.dest));
    engine.shutdown().await.unwrap();
}

/// Every file under `dir` with its size, by its `/`-joined path inside it.
fn sizes(dir: &Path) -> BTreeMap<String, usize> {
    tree(dir).into_iter().map(|(k, v)| (k, v.len())).collect()
}

// Progress across restarts.

#[tokio::test(flavor = "multi_thread")]
async fn paused_torrent_keeps_its_progress_across_restart() {
    // About six seconds at 64 KiB/s.
    let (seeder, a, b) = slow_seeder((300 * 1024, 60 * 1024)).await;
    let total = (a.len() + b.len()) as u64;
    let rs = Restarts::new(vec![seeder.addr]);
    let engine = rs.open(true, policy(Some(0.0), None)).await;
    let mut events = engine.subscribe();
    let id = engine.add(rs.request(&seeder.magnet)).await.unwrap();
    events_until(&mut events, |e| {
        progress_of(e, id).is_some_and(|d| d >= 64 * 1024)
    })
    .await;
    engine.pause(id).await.unwrap();
    let paused = engine.snapshot().into_iter().find(|v| v.id == id).unwrap();
    assert!(paused.downloaded < total, "finished before the pause");
    engine.shutdown().await.unwrap();
    drop(engine);

    let engine = rs.open(false, policy(Some(0.0), None)).await;
    let view = engine.snapshot().into_iter().find(|v| v.id == id).unwrap();
    assert_eq!(view.state, JobState::Paused);
    assert_eq!(view.total, Some(total));
    assert_eq!(view.downloaded, paused.downloaded);
    assert!(view.downloaded >= 64 * 1024);
    engine.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn interrupted_seeding_torrent_shows_it_is_downloaded() {
    let (seeder, a, b) = seeder().await;
    let total = (a.len() + b.len()) as u64;
    let rs = Restarts::new(vec![seeder.addr]);
    let engine = rs.open(true, policy(None, None)).await;
    let mut events = engine.subscribe();
    let id = engine.add(rs.request(&seeder.magnet)).await.unwrap();
    events_until(&mut events, |e| updated_to(e, id, JobState::Seeding)).await;
    engine.shutdown().await.unwrap();
    drop(engine);

    let engine = rs.open(false, policy(None, None)).await;
    let view = engine.snapshot().into_iter().find(|v| v.id == id).unwrap();
    assert_eq!(view.state, JobState::Queued);
    assert_eq!((view.downloaded, view.total), (total, Some(total)));
    engine.shutdown().await.unwrap();
}

/// Free space for `space_after_restart`, which alone reads it.
static RESTART_SPACE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(u64::MAX);

fn restart_space(_: &Path) -> std::io::Result<u64> {
    Ok(RESTART_SPACE.load(std::sync::atomic::Ordering::SeqCst))
}

#[tokio::test(flavor = "multi_thread")]
async fn space_after_restart_counts_what_is_downloaded() {
    let file = data(2 * MIB, 22);
    let total = file.len() as u64;
    // About eight seconds for the whole file.
    let seeder = TorrentSeeder::start("big", &[("big.bin", file.clone())], Some(256 * 1024)).await;
    let rs = Restarts::new(vec![seeder.addr]);
    let engine = rs.open(true, policy(Some(0.0), None)).await;
    let mut events = engine.subscribe();
    let id = engine.add(rs.request(&seeder.magnet)).await.unwrap();
    let seen = events_until(&mut events, |e| {
        progress_of(e, id).is_some_and(|d| d >= MIB as u64)
    })
    .await;
    let before = seen.iter().rev().find_map(|e| progress_of(e, id)).unwrap();
    assert!(before < total, "finished before the restart");
    engine.shutdown().await.unwrap();
    drop(engine);

    // Room for what is left, not for the whole torrent.
    RESTART_SPACE.store(
        SPACE_MARGIN + (total - before) + 256 * 1024,
        std::sync::atomic::Ordering::SeqCst,
    );
    let mut config = engine_config(rs.tmp.path(), vec![seeder.addr], restart_space);
    // Started by hand, before anything else starts the torrent session.
    config.autostart = false;
    let engine = Engine::open(config).await.unwrap();
    engine.start_now(id).await.unwrap();
    assert_eq!(wait_job(&engine, id).await, JobState::Completed);
    let view = engine.snapshot().into_iter().find(|v| v.id == id).unwrap();
    assert_eq!(view.error, None);
    assert!(fs::read(rs.dest.join(&view.file_name)).unwrap() == file);
    engine.shutdown().await.unwrap();
}

// Restarts as the product restarts: librqbit restores its torrents and the
// reconcile only pauses them.

#[tokio::test(flavor = "multi_thread")]
async fn restart_in_place_shares_from_local_data() {
    let (seeder, a, b) = seeder().await;
    let total = (a.len() + b.len()) as u64;
    let rs = Restarts {
        in_place: true,
        ..Restarts::new(vec![seeder.addr])
    };
    let engine = rs.open(true, policy(None, None)).await;
    let mut events = engine.subscribe();
    let id = engine.add(rs.request(&seeder.magnet)).await.unwrap();
    events_until(&mut events, |e| updated_to(e, id, JobState::Seeding)).await;
    engine.shutdown().await.unwrap();
    drop(engine);
    let ids = rs.session_ids();
    assert_eq!(ids.len(), 1);
    // Nobody to download from: whatever it has must come from its files.
    drop(seeder);

    let engine = rs.open(true, policy(None, None)).await;
    let mut events = engine.subscribe();
    let seen = events_until(&mut events, |e| updated_to(e, id, JobState::Seeding)).await;
    for event in &seen {
        if let Some(downloaded) = progress_of(event, id) {
            assert_eq!(downloaded, total, "it had to get pieces again");
        }
    }
    let view = engine.snapshot().into_iter().find(|v| v.id == id).unwrap();
    assert_eq!((view.downloaded, view.error), (total, None));
    assert!(tree(&rs.dest.join("pack")) == files_of(&[("a.bin", a), ("dir/b.bin", b)]));
    // It is the torrent librqbit restored, not one added again (under the
    // next free id).
    assert_eq!(rs.session_ids(), ids);

    // And the policy still applies: done as soon as sharing is not wanted.
    let mut settings = engine.settings();
    settings.torrent.stop_ratio = Some(0.0);
    engine.set_settings(settings).await.unwrap();
    assert_eq!(wait_job(&engine, id).await, JobState::Completed);
    engine.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn seed_time_counts_across_a_restart() {
    let (seeder, _, _) = seeder().await;
    let rs = Restarts {
        in_place: true,
        ..Restarts::new(vec![seeder.addr])
    };
    let engine = rs.open(true, policy(None, Some(6))).await;
    let mut events = engine.subscribe();
    let id = engine.add(rs.request(&seeder.magnet)).await.unwrap();
    events_until(&mut events, |e| updated_to(e, id, JobState::Seeding)).await;
    let seeding = Instant::now();
    engine.shutdown().await.unwrap();
    drop(engine);
    let off = Duration::from_secs(4).saturating_sub(seeding.elapsed());
    tokio::time::sleep(off).await;

    let reopened = Instant::now();
    let engine = rs.open(true, policy(None, Some(6))).await;
    assert_eq!(wait_job(&engine, id).await, JobState::Completed);
    // Six seconds from when it started sharing (counted in whole seconds),
    // not six more from the restart.
    let (shared, after) = (seeding.elapsed(), reopened.elapsed());
    assert!(
        shared >= Duration::from_millis(4500),
        "shared for {shared:?}"
    );
    assert!(
        after < Duration::from_secs(5),
        "{after:?} after the restart"
    );
    engine.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn sharing_again_after_a_restart_does_not_fire_the_when_done_action() {
    let (seeder, _, _) = seeder().await;
    let rs = Restarts::new(vec![seeder.addr]);
    let engine = rs.open(true, policy(None, None)).await;
    let mut settings = engine.settings();
    settings.when_done = WhenDone::Notify;
    engine.set_settings(settings).await.unwrap();
    let mut events = engine.subscribe();
    let id = engine.add(rs.request(&seeder.magnet)).await.unwrap();
    // The download finishing fires it once.
    events_until(&mut events, |e| matches!(e, Event::WhenDoneFired { .. })).await;
    engine.shutdown().await.unwrap();
    drop(engine);

    // The same settings, as saved: it shares again at once.
    let engine = Engine::open(engine_config(
        rs.tmp.path(),
        rs.peers.clone(),
        available_space,
    ))
    .await
    .unwrap();
    let mut events = engine.subscribe();
    let seen = events_until(&mut events, |e| matches!(e, Event::QueueIdle)).await;
    assert_eq!(engine.settings().when_done, WhenDone::Notify);
    assert!(
        seen.iter().any(|e| updated_to(e, id, JobState::Seeding)) || {
            let view = engine.snapshot().into_iter().find(|v| v.id == id).unwrap();
            view.state == JobState::Seeding
        }
    );
    // Past the countdown: nothing finished in this run.
    tokio::time::sleep(Timing::fast_for_tests().when_done_countdown * 3).await;
    while let Ok(event) = events.try_recv() {
        assert!(
            !matches!(
                event,
                Event::WhenDoneCountdown { .. } | Event::WhenDoneFired { .. }
            ),
            "{event:?}"
        );
    }
    engine.shutdown().await.unwrap();
}

// Not on Windows: librqbit's listener sets SO_REUSEADDR, which there lets it
// bind a port a plain socket already holds.
#[cfg(not(windows))]
#[tokio::test(flavor = "multi_thread")]
async fn a_taken_port_falls_back_to_a_random_one() {
    let tmp = tempfile::tempdir().unwrap();
    let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = taken.local_addr().unwrap().port();
    let store = Store::open(&tmp.path().join("taurus.db")).unwrap();
    let settings = TorrentSettings {
        listen_port: Some(port),
        ..TorrentSettings::default()
    };
    let hub = TorrentHub::new(
        tmp.path(),
        &settings,
        Some(TorrentTestNet::default()),
        store,
    );
    hub.session().await.unwrap();
    let addr = hub.listen_addr().expect("listens");
    assert_ne!(addr.port(), port);
    assert_ne!(addr.port(), 0);
    hub.shutdown().await;
}

/// librqbit takes the SOCKS proxy URL the engine hands it (one naming no
/// port gets 1080, which librqbit needs).
#[tokio::test(flavor = "multi_thread")]
async fn the_session_starts_with_a_socks_proxy() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(&tmp.path().join("taurus.db")).unwrap();
    let proxy = taurus_core::ProxySetting::Manual {
        url: "socks5h://127.0.0.1".to_string(),
    };
    let url = taurus_core::network::torrent_proxy(&proxy);
    assert_eq!(url.as_deref(), Some("socks5h://127.0.0.1:1080"));
    let hub = TorrentHub::new(
        tmp.path(),
        &TorrentSettings::default(),
        Some(TorrentTestNet::default()),
        store,
    );
    hub.set_proxy(url);
    hub.session().await.unwrap();
    hub.shutdown().await;
}
