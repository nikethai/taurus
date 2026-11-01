//! TorrentHub and the torrent job runner against real local librqbit
//! sessions on 127.0.0.1.

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
use taurus_core::system::{SpaceSource, available_space};
use taurus_core::torrent::TorrentFile;
use taurus_core::torrent::hub::{TorrentHub, TorrentTestNet};
use taurus_core::torrent::job::{TorrentJobContext, run_torrent_job};
use taurus_core::torrent::source::{Source, classify};
use taurus_core::{ErrorKind, JobState, Timing, TorrentSettings};
use taurus_testkit::{
    FileSpec, TorrentLeecher, TorrentSeeder, content, make_torrent_bytes, test_session_options,
};
use tempfile::TempDir;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

const TIMEOUT: Duration = Duration::from_secs(20);

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
        settings,
        space,
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
    let (ctx, _rx, _cancel) = context(&rig, id, policy(Some(0.0), None), available_space);

    let JobOutcome::Completed(out) = run(ctx).await else {
        panic!("not completed");
    };
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
    // Sharing stopped, and the runner let go of the torrent.
    assert!(rig.hub.handle(&seeder.info_hash).unwrap().is_paused());
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
        .filter(|(_, m)| matches!(m, JobMsg::Seeding { id: s } if *s == id))
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
    rig.store.commit_torrent_progress(id, 1_000).unwrap();
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
