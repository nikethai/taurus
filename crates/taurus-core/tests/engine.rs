use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use taurus_core::store::{NewJob, Store};
use taurus_core::{AddRequest, Engine, EngineConfig, ErrorKind, Event, JobId, JobState, JobView};
use taurus_testkit::{FileSpec, TestServer, content, sha256_hex};
use tokio::sync::broadcast;

const MIB: usize = 1024 * 1024;
const PATH: &str = "/x.bin";
const TIMEOUT: Duration = Duration::from_secs(30);

struct Fixture {
    tmp: tempfile::TempDir,
    dest: PathBuf,
    server: TestServer,
}

impl Fixture {
    async fn new() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("out");
        fs::create_dir(&dest).unwrap();
        Fixture {
            tmp,
            dest,
            server: TestServer::start().await,
        }
    }

    fn data_dir(&self) -> PathBuf {
        self.tmp.path().join("data")
    }

    async fn engine(&self) -> Engine {
        Engine::open(EngineConfig {
            data_dir: self.data_dir(),
        })
        .await
        .unwrap()
    }

    fn request(&self, url: &str) -> AddRequest {
        AddRequest {
            url: url.to_string(),
            dest_dir: self.dest.clone(),
            file_name: None,
            connections: None,
        }
    }

    fn tapa(&self) -> PathBuf {
        self.dest.join("x.bin.tapa")
    }
}

/// Capped at 4 MiB/s for the path: 20 MiB takes about 5 s.
fn throttled(size: usize) -> FileSpec {
    let mut spec = FileSpec::new(size);
    spec.bytes_per_sec = Some(4 * MIB as u64);
    spec
}

fn unthrottled(spec: &FileSpec) -> FileSpec {
    let mut fast = spec.clone();
    fast.bytes_per_sec = None;
    fast
}

async fn wait(engine: &Engine, id: JobId) -> JobState {
    tokio::time::timeout(TIMEOUT, engine.wait(id))
        .await
        .expect("job did not settle in 30 s")
}

async fn next_event(events: &mut broadcast::Receiver<Event>) -> Event {
    tokio::time::timeout(TIMEOUT, events.recv())
        .await
        .expect("no event in 30 s")
        .expect("event channel closed or lagged")
}

fn view(engine: &Engine, id: JobId) -> JobView {
    engine
        .snapshot()
        .into_iter()
        .find(|v| v.id == id)
        .expect("job is in the snapshot")
}

fn assert_file(path: &Path, spec: &FileSpec) {
    let got = fs::read(path).unwrap();
    assert_eq!(got.len(), spec.size);
    assert_eq!(sha256_hex(&got), sha256_hex(&content(spec)));
}

/// Adds a throttled job and pauses it after about 1 s.
async fn add_and_pause(fx: &Fixture, engine: &Engine, spec: &FileSpec) -> JobId {
    let url = fx.server.serve(PATH, spec.clone());
    let id = engine.add(fx.request(&url)).await.unwrap();
    tokio::time::sleep(Duration::from_secs(1)).await;
    engine.pause(id).await.unwrap();
    assert_eq!(wait(engine, id).await, JobState::Paused);
    id
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn add_downloads_and_emits_events() {
    let fx = Fixture::new().await;
    let engine = fx.engine().await;
    let mut events = engine.subscribe();
    let spec = FileSpec::new(6 * MIB);
    let url = fx.server.serve(PATH, spec.clone());

    let id = engine.add(fx.request(&url)).await.unwrap();
    match next_event(&mut events).await {
        Event::JobAdded(v) => {
            assert_eq!(v.id, id);
            assert_eq!(v.file_name, "x.bin");
            assert_eq!(v.total, Some(spec.size as u64));
            assert_eq!(v.state, JobState::Downloading);
        }
        other => panic!("expected JobAdded, got {other:?}"),
    }
    let mut progress = 0;
    loop {
        match next_event(&mut events).await {
            Event::Progress { id: pid, .. } if pid == id => progress += 1,
            Event::JobUpdated(v) if v.id == id && v.state == JobState::Completed => break,
            Event::JobUpdated(v) if v.id == id && v.state == JobState::Downloading => {}
            other => panic!("unexpected event {other:?}"),
        }
    }
    assert!(progress >= 1, "no Progress event");

    assert_eq!(wait(&engine, id).await, JobState::Completed);
    let v = view(&engine, id);
    assert_eq!(v.state, JobState::Completed);
    assert_eq!(v.total, Some(spec.size as u64));
    assert_eq!(v.downloaded, spec.size as u64);
    assert_eq!(v.speed_bps, 0);
    assert_eq!(v.error, None);
    assert_file(&fx.dest.join("x.bin"), &spec);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pause_then_resume_completes() {
    let fx = Fixture::new().await;
    let engine = fx.engine().await;
    let spec = throttled(20 * MIB);
    let id = add_and_pause(&fx, &engine, &spec).await;

    let v = view(&engine, id);
    assert_eq!(v.state, JobState::Paused);
    assert!(v.downloaded > 0 && v.downloaded < spec.size as u64, "{v:?}");
    assert_eq!(v.speed_bps, 0);
    assert!(fx.tapa().exists());

    let served_before = fx.server.bytes_served(PATH);
    fx.server.replace(PATH, unthrottled(&spec));
    engine.resume(id).await.unwrap();
    assert_eq!(wait(&engine, id).await, JobState::Completed);
    assert_file(&fx.dest.join("x.bin"), &spec);
    // Resumed, not restarted.
    assert!(fx.server.bytes_served(PATH) - served_before < spec.size as u64);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reopen_restores_paused_job() {
    let fx = Fixture::new().await;
    let spec = throttled(20 * MIB);
    let id = {
        let engine = fx.engine().await;
        let id = add_and_pause(&fx, &engine, &spec).await;
        engine.shutdown().await.unwrap();
        id
    };

    let engine = fx.engine().await;
    let snapshot = engine.snapshot();
    assert_eq!(snapshot.len(), 1);
    let v = &snapshot[0];
    assert_eq!(v.id, id);
    assert_eq!(v.state, JobState::Paused);
    assert!(v.downloaded > 0 && v.downloaded < spec.size as u64, "{v:?}");
    assert_eq!(v.file_name, "x.bin");

    fx.server.replace(PATH, unthrottled(&spec));
    engine.resume(id).await.unwrap();
    assert_eq!(wait(&engine, id).await, JobState::Completed);
    assert_file(&fx.dest.join("x.bin"), &spec);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn changed_etag_restarts_from_zero() {
    let fx = Fixture::new().await;
    let engine = fx.engine().await;
    let mut spec = throttled(20 * MIB);
    spec.etag = Some("\"v1\"".into());
    let id = add_and_pause(&fx, &engine, &spec).await;
    assert!(view(&engine, id).downloaded > 0);

    // Same size, other content.
    let mut changed = unthrottled(&spec);
    changed.seed = 2;
    changed.etag = Some("\"v2\"".into());
    fx.server.replace(PATH, changed.clone());
    engine.resume(id).await.unwrap();
    assert_eq!(wait(&engine, id).await, JobState::Completed);

    assert_file(&fx.dest.join("x.bin"), &changed);
    let v = view(&engine, id);
    assert_eq!(v.downloaded, changed.size as u64);
    assert_eq!(
        v.error.map(|(kind, _)| kind),
        Some(ErrorKind::FileChangedOnServer)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn same_name_twice_gets_suffix() {
    let fx = Fixture::new().await;
    let engine = fx.engine().await;
    let spec = FileSpec::new(3 * MIB);
    let url = fx.server.serve(PATH, spec.clone());

    // Concurrent adds: the name reservation must not race.
    let (a, b) = tokio::join!(engine.add(fx.request(&url)), engine.add(fx.request(&url)));
    let (a, b) = (a.unwrap(), b.unwrap());
    assert_eq!(wait(&engine, a).await, JobState::Completed);
    assert_eq!(wait(&engine, b).await, JobState::Completed);

    let names: HashSet<String> = [view(&engine, a), view(&engine, b)]
        .into_iter()
        .map(|v| v.file_name)
        .collect();
    let expected: HashSet<String> = ["x.bin".to_string(), "x (1).bin".to_string()].into();
    assert_eq!(names, expected);
    let on_disk: HashSet<String> = fs::read_dir(&fx.dest)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(on_disk, expected);
    assert_file(&fx.dest.join("x.bin"), &spec);
    assert_file(&fx.dest.join("x (1).bin"), &spec);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn missing_dest_dir_fails_with_folder_unavailable() {
    let fx = Fixture::new().await;
    let engine = fx.engine().await;
    let mut events = engine.subscribe();
    let url = fx.server.serve(PATH, FileSpec::new(MIB));
    let missing = fx.tmp.path().join("does-not-exist");

    let mut req = fx.request(&url);
    req.dest_dir = missing.clone();
    let err = engine.add(req).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::FolderUnavailable);
    assert!(engine.snapshot().is_empty());
    assert!(matches!(
        events.try_recv(),
        Err(broadcast::error::TryRecvError::Empty)
    ));
    assert!(!missing.exists());
    assert_eq!(fx.server.requests(PATH), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn remove_with_files_deletes_partial() {
    let fx = Fixture::new().await;
    let engine = fx.engine().await;
    let id = add_and_pause(&fx, &engine, &throttled(20 * MIB)).await;
    assert!(fx.tapa().exists());
    let mut events = engine.subscribe();

    engine.remove(id, true).await.unwrap();
    assert!(!fx.tapa().exists());
    assert!(engine.snapshot().is_empty());
    match next_event(&mut events).await {
        Event::JobRemoved(removed) => assert_eq!(removed, id),
        other => panic!("expected JobRemoved, got {other:?}"),
    }
    assert_eq!(
        engine.remove(id, true).await.unwrap_err().kind,
        ErrorKind::Unknown
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shutdown_pauses_running_jobs_and_stops_engine() {
    let fx = Fixture::new().await;
    let engine = fx.engine().await;
    let url = fx.server.serve(PATH, throttled(20 * MIB));
    let id = engine.add(fx.request(&url)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;

    engine.shutdown().await.unwrap();
    assert_eq!(view(&engine, id).state, JobState::Paused);
    assert_eq!(wait(&engine, id).await, JobState::Paused);
    let err = engine.resume(id).await.unwrap_err();
    assert_eq!(
        (err.kind, err.detail.as_str()),
        (ErrorKind::Unknown, "engine stopped")
    );
    assert!(engine.add(fx.request(&url)).await.is_err());
    drop(engine);

    let engine = fx.engine().await;
    let v = view(&engine, id);
    assert_eq!(v.state, JobState::Paused);
    assert!(v.downloaded > 0, "{v:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn open_turns_interrupted_jobs_into_paused() {
    let fx = Fixture::new().await;
    // A process killed mid-download leaves its job recorded as Downloading.
    fs::create_dir_all(fx.data_dir()).unwrap();
    {
        let store = Store::open(&fx.data_dir().join("taurus.db")).unwrap();
        for state in [JobState::Downloading, JobState::Queued] {
            let id = store
                .insert_job(&NewJob {
                    url: "http://127.0.0.1:9/x.bin".into(),
                    dest_dir: fx.dest.clone(),
                    file_name: "x.bin".into(),
                    connections: 4,
                })
                .unwrap();
            let mut job = store.get_job(id).unwrap().unwrap();
            job.state = state;
            store.update_job(&job).unwrap();
        }
    }

    let engine = fx.engine().await;
    let snapshot = engine.snapshot();
    assert_eq!(snapshot.len(), 2);
    assert!(
        snapshot.iter().all(|v| v.state == JobState::Paused),
        "{snapshot:?}"
    );
    engine.shutdown().await.unwrap();

    let store = Store::open(&fx.data_dir().join("taurus.db")).unwrap();
    let states: Vec<JobState> = store.list_jobs().unwrap().iter().map(|j| j.state).collect();
    assert_eq!(states, vec![JobState::Paused, JobState::Paused]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn remove_with_files_spares_another_jobs_file() {
    let fx = Fixture::new().await;
    let engine = fx.engine().await;
    let first = FileSpec::new(MIB);
    let url = fx.server.serve(PATH, first.clone());
    let a = engine.add(fx.request(&url)).await.unwrap();
    assert_eq!(wait(&engine, a).await, JobState::Completed);
    // The user moves the finished file away, so the name is free again.
    fs::rename(fx.dest.join("x.bin"), fx.tmp.path().join("moved.bin")).unwrap();

    let mut second = FileSpec::new(MIB);
    second.seed = 2;
    fx.server.replace(PATH, second.clone());
    let b = engine.add(fx.request(&url)).await.unwrap();
    assert_eq!(wait(&engine, b).await, JobState::Completed);
    assert_eq!(view(&engine, b).file_name, "x.bin");

    engine.remove(a, true).await.unwrap();
    assert_file(&fx.dest.join("x.bin"), &second);
    assert_eq!(engine.snapshot().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn remove_with_files_spares_a_replaced_file() {
    let fx = Fixture::new().await;
    let engine = fx.engine().await;
    let url = fx.server.serve(PATH, FileSpec::new(MIB));
    let a = engine.add(fx.request(&url)).await.unwrap();
    let b = engine.add(fx.request(&url)).await.unwrap();
    assert_eq!(wait(&engine, a).await, JobState::Completed);
    assert_eq!(wait(&engine, b).await, JobState::Completed);
    let replaced = fx.dest.join(&view(&engine, a).file_name);
    let downloaded = fx.dest.join(&view(&engine, b).file_name);
    // Not the size that was downloaded: someone else's file now.
    fs::write(&replaced, b"user data").unwrap();

    engine.remove(a, true).await.unwrap();
    assert_eq!(fs::read(&replaced).unwrap(), b"user data");
    engine.remove(b, true).await.unwrap();
    assert!(!downloaded.exists());
    assert!(engine.snapshot().is_empty());
}
