use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use taurus_core::store::{JobRecord, NewJob, Store};
use taurus_core::{
    AddRequest, Engine, EngineConfig, ErrorKind, Event, JobId, JobState, JobView, Settings,
    SpeedLimit,
};
use taurus_testkit::{FailAfter, FileSpec, TestServer, content, sha256_hex};
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
        Engine::open(EngineConfig::new(self.data_dir()))
            .await
            .unwrap()
    }

    fn request(&self, url: &str) -> AddRequest {
        AddRequest {
            url: url.to_string(),
            dest_dir: self.dest.clone(),
            ..AddRequest::default()
        }
    }

    /// The job as stored, read beside the running engine.
    fn stored(&self, id: JobId) -> JobRecord {
        Store::open(&self.data_dir().join("taurus.db"))
            .unwrap()
            .get_job(id)
            .unwrap()
            .expect("job is stored")
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
            assert_eq!(v.state, JobState::Queued);
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
        for state in [JobState::Downloading, JobState::Paused] {
            let id = store
                .insert_job(&NewJob {
                    url: "http://127.0.0.1:9/x.bin".into(),
                    dest_dir: fx.dest.clone(),
                    file_name: "x.bin".into(),
                    connections: 4,
                    mirrors: Vec::new(),
                    checksum: None,
                    referer: None,
                    queue_pos: 0,
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn second_engine_on_same_data_dir_is_refused() {
    let fx = Fixture::new().await;
    let engine = fx.engine().await;
    let url = fx.server.serve(PATH, throttled(20 * MIB));
    let id = engine.add(fx.request(&url)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;

    let config = EngineConfig::new(fx.data_dir());
    let err = match Engine::open(config).await {
        Ok(_) => panic!("a second engine opened the same data dir"),
        Err(e) => e,
    };
    assert_eq!(err.kind, ErrorKind::Unknown);
    assert!(err.is_instance_locked(), "{err}");
    assert_eq!(
        err.detail,
        format!(
            "another taurus instance is using {}",
            fx.data_dir().display()
        )
    );

    // The running job is untouched and still readable from beside it.
    let config = EngineConfig::new(fx.data_dir());
    let stored = Engine::read_snapshot(&config).unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!((stored[0].id, stored[0].state), (id, JobState::Downloading));
    assert_eq!(view(&engine, id).state, JobState::Downloading);

    // Shutting down releases the data dir.
    engine.shutdown().await.unwrap();
    let engine = fx.engine().await;
    assert_eq!(view(&engine, id).state, JobState::Paused);
    let stored = Engine::read_snapshot(&config).unwrap();
    assert_eq!(stored[0].state, JobState::Paused);
    assert!(stored[0].downloaded > 0, "{stored:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn read_snapshot_without_database_is_empty() {
    let fx = Fixture::new().await;
    let config = EngineConfig::new(fx.data_dir());
    assert!(Engine::read_snapshot(&config).unwrap().is_empty());
    assert!(!fx.data_dir().exists());

    // A closed engine's database reads back too.
    let engine = fx.engine().await;
    let url = fx.server.serve(PATH, FileSpec::new(MIB));
    let id = engine.add(fx.request(&url)).await.unwrap();
    assert_eq!(wait(&engine, id).await, JobState::Completed);
    engine.shutdown().await.unwrap();
    let stored = Engine::read_snapshot(&config).unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].state, JobState::Completed);
    assert_eq!(stored[0].downloaded, MIB as u64);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn add_through_confirm_form_downloads_the_file() {
    let fx = Fixture::new().await;
    let engine = fx.engine().await;
    let form = r#"<form action="/x.bin"><input type="hidden" name="confirm" value="t"></form>"#;
    let page = fx.server.serve_page("/view", 200, &[], form);
    let spec = FileSpec::new(3 * MIB);
    let file = fx.server.serve(PATH, spec.clone());

    let id = engine.add(fx.request(&page)).await.unwrap();
    assert_eq!(wait(&engine, id).await, JobState::Completed);
    // Segments re-follow the download link, not the page.
    assert_eq!(view(&engine, id).url, format!("{file}?confirm=t"));
    assert_eq!(fx.server.requests("/view"), 1);
    assert_file(&fx.dest.join("x.bin"), &spec);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn add_web_page_fails_and_records_nothing() {
    let fx = Fixture::new().await;
    let engine = fx.engine().await;
    let page = fx
        .server
        .serve_page("/blog/", 200, &[], "<html><body>Hello</body></html>");

    let err = engine.add(fx.request(&page)).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::SiteNotSupported);
    assert!(engine.snapshot().is_empty());
}

fn with_speed(speed: SpeedLimit) -> Settings {
    Settings {
        speed,
        ..Settings::default()
    }
}

/// The measured peak saved in the closed engine's database.
fn stored_peak(fx: &Fixture) -> Option<(u64, i64)> {
    let store = Store::open(&fx.data_dir().join("taurus.db")).unwrap();
    store.measured_peak().unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn settings_are_saved_and_announced() {
    let fx = Fixture::new().await;
    let engine = fx.engine().await;
    assert_eq!(engine.settings(), Settings::default());
    let mut events = engine.subscribe();
    let settings = Settings {
        max_active: 5,
        speed: SpeedLimit::Gentle,
        ..Settings::default()
    };

    engine.set_settings(settings.clone()).await.unwrap();
    assert_eq!(engine.settings(), settings);
    match next_event(&mut events).await {
        Event::SettingsChanged(s) => assert_eq!(s, settings),
        other => panic!("expected SettingsChanged, got {other:?}"),
    }
    engine.shutdown().await.unwrap();

    let engine = fx.engine().await;
    assert_eq!(engine.settings(), settings);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn custom_limit_slows_download() {
    let fx = Fixture::new().await;
    let engine = fx.engine().await;
    let custom = SpeedLimit::Custom {
        bytes_per_sec: 2 * MIB as u64,
    };
    engine.set_settings(with_speed(custom)).await.unwrap();
    let spec = FileSpec::new(6 * MIB);
    let url = fx.server.serve(PATH, spec.clone());

    let start = Instant::now();
    let id = engine.add(fx.request(&url)).await.unwrap();
    assert_eq!(wait(&engine, id).await, JobState::Completed);
    let elapsed = start.elapsed();
    assert!(elapsed >= Duration::from_millis(2500), "took {elapsed:?}");
    assert_file(&fx.dest.join("x.bin"), &spec);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn limit_change_applies_to_running_job() {
    let fx = Fixture::new().await;
    let engine = fx.engine().await;
    let custom = SpeedLimit::Custom {
        bytes_per_sec: MIB as u64,
    };
    engine.set_settings(with_speed(custom)).await.unwrap();
    let spec = FileSpec::new(8 * MIB);
    let url = fx.server.serve(PATH, spec.clone());

    let id = engine.add(fx.request(&url)).await.unwrap();
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(view(&engine, id).state, JobState::Downloading);
    let switched = Instant::now();
    engine
        .set_settings(with_speed(SpeedLimit::Fastest))
        .await
        .unwrap();
    assert_eq!(wait(&engine, id).await, JobState::Completed);
    let elapsed = switched.elapsed();
    assert!(elapsed < Duration::from_secs(4), "took {elapsed:?}");
    assert_file(&fx.dest.join("x.bin"), &spec);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stale_peak_is_replaced_by_a_lower_measurement() {
    let fx = Fixture::new().await;
    fs::create_dir_all(fx.data_dir()).unwrap();
    // Measured long ago, on a far faster line.
    let stale = (1_000_000_000_000, 0);
    Store::open(&fx.data_dir().join("taurus.db"))
        .unwrap()
        .save_measured_peak(stale.0, stale.1)
        .unwrap();
    let engine = fx.engine().await;
    let url = fx.server.serve(PATH, throttled(8 * MIB));

    let id = engine.add(fx.request(&url)).await.unwrap();
    assert_eq!(wait(&engine, id).await, JobState::Completed);
    engine.shutdown().await.unwrap();

    let (bps, at) = stored_peak(&fx).expect("a peak is stored");
    // The path is capped at 4 MiB/s.
    assert!((MIB as u64..8 * MIB as u64).contains(&bps), "peak {bps}");
    assert!(at > stale.1, "measured at {at}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fresh_peak_is_kept_when_not_beaten() {
    let fx = Fixture::new().await;
    fs::create_dir_all(fx.data_dir()).unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let fresh = (1_000_000_000_000, now - 3600);
    Store::open(&fx.data_dir().join("taurus.db"))
        .unwrap()
        .save_measured_peak(fresh.0, fresh.1)
        .unwrap();
    let engine = fx.engine().await;
    let url = fx.server.serve(PATH, throttled(8 * MIB));

    let id = engine.add(fx.request(&url)).await.unwrap();
    assert_eq!(wait(&engine, id).await, JobState::Completed);
    engine.shutdown().await.unwrap();
    assert_eq!(stored_peak(&fx), Some(fresh));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn peak_is_not_measured_under_a_limit() {
    let fx = Fixture::new().await;
    let engine = fx.engine().await;
    // Above the server's 4 MiB/s, so the limit never slows this download.
    let custom = SpeedLimit::Custom {
        bytes_per_sec: 64 * MIB as u64,
    };
    engine.set_settings(with_speed(custom)).await.unwrap();
    let url = fx.server.serve(PATH, throttled(8 * MIB));

    let id = engine.add(fx.request(&url)).await.unwrap();
    assert_eq!(wait(&engine, id).await, JobState::Completed);
    engine.shutdown().await.unwrap();
    assert_eq!(stored_peak(&fx), None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pause_is_not_held_up_by_a_slow_limit() {
    let fx = Fixture::new().await;
    let engine = fx.engine().await;
    // 1 KiB/s: a worker waits about a minute for the bucket per 64 KiB.
    let crawl = SpeedLimit::Custom {
        bytes_per_sec: 1024,
    };
    engine.set_settings(with_speed(crawl)).await.unwrap();
    let url = fx.server.serve(PATH, FileSpec::new(6 * MIB));
    let id = engine.add(fx.request(&url)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;

    let start = Instant::now();
    engine.pause(id).await.unwrap();
    let elapsed = start.elapsed();
    assert!(elapsed < Duration::from_secs(5), "pause took {elapsed:?}");
    assert_eq!(view(&engine, id).state, JobState::Paused);
}

/// Every request after the first `after_requests` answers `status`.
fn fail_after(after_requests: usize, status: u16) -> FailAfter {
    FailAfter {
        after_requests,
        status,
        headers: Vec::new(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mirrors_share_the_work() {
    let fx = Fixture::new().await;
    let engine = fx.engine().await;
    let spec = FileSpec::new(16 * MIB);
    let url = fx.server.serve(PATH, spec.clone());
    let mirror = TestServer::start().await;
    let mirror_url = mirror.serve(PATH, spec.clone());

    let id = engine
        .add(AddRequest {
            connections: Some(4),
            mirrors: vec![mirror_url.clone()],
            ..fx.request(&url)
        })
        .await
        .unwrap();
    assert_eq!(wait(&engine, id).await, JobState::Completed);
    assert_file(&fx.dest.join("x.bin"), &spec);
    // More than the 1-byte probes: each served whole segments.
    assert!(fx.server.bytes_served(PATH) > MIB as u64);
    assert!(mirror.bytes_served(PATH) > MIB as u64);
    assert_eq!(fx.stored(id).mirrors, [mirror_url]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn broken_mirror_is_skipped() {
    let fx = Fixture::new().await;
    let engine = fx.engine().await;
    let spec = FileSpec::new(8 * MIB);
    let url = fx.server.serve(PATH, spec.clone());
    // Passes the check at add, then answers 404.
    let mut broken = spec.clone();
    broken.fail_after = Some(fail_after(1, 404));
    let mirror = TestServer::start().await;
    let mirror_url = mirror.serve(PATH, broken);

    let id = engine
        .add(AddRequest {
            connections: Some(4),
            mirrors: vec![mirror_url.clone()],
            ..fx.request(&url)
        })
        .await
        .unwrap();
    assert_eq!(wait(&engine, id).await, JobState::Completed);
    assert_file(&fx.dest.join("x.bin"), &spec);
    assert_eq!(fx.stored(id).mirrors, [mirror_url]);
    assert!(mirror.requests(PATH) > 1, "the mirror was not tried");
    assert_eq!(view(&engine, id).error, None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn all_mirrors_broken_reports_primary_error() {
    let fx = Fixture::new().await;
    let engine = fx.engine().await;
    let mut primary = FileSpec::new(4 * MIB);
    primary.fail_after = Some(fail_after(1, 410));
    let url = fx.server.serve(PATH, primary.clone());
    let mut broken = primary.clone();
    broken.fail_after = Some(fail_after(1, 404));
    let mirror = TestServer::start().await;
    let mirror_url = mirror.serve(PATH, broken);

    let id = engine
        .add(AddRequest {
            connections: Some(4),
            mirrors: vec![mirror_url],
            ..fx.request(&url)
        })
        .await
        .unwrap();
    assert_eq!(wait(&engine, id).await, JobState::Failed);
    assert!(mirror.requests(PATH) > 1, "the mirror was not tried");
    let (kind, detail) = view(&engine, id).error.expect("an error is recorded");
    assert_eq!(kind, ErrorKind::LinkExpired);
    assert!(detail.contains("410"), "{detail}");
    let stored = fx.stored(id).error.expect("an error is stored");
    assert_eq!(stored, (kind, detail));
    assert!(!fx.dest.join("x.bin").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mismatched_mirror_dropped_at_add() {
    let fx = Fixture::new().await;
    let engine = fx.engine().await;
    let mut spec = FileSpec::new(4 * MIB);
    spec.etag = Some("\"v1\"".into());
    let url = fx.server.serve(PATH, spec.clone());
    let mirror = TestServer::start().await;
    let mut longer = spec.clone();
    longer.size += 1;
    let mut other_etag = spec.clone();
    other_etag.etag = Some("\"v2\"".into());
    let mut gone = spec.clone();
    gone.fail_first = vec![404];
    let mirrors = vec![
        mirror.serve("/longer.bin", longer),
        mirror.serve("/etag.bin", other_etag),
        mirror.serve("/gone.bin", gone),
    ];

    let id = engine
        .add(AddRequest {
            mirrors,
            ..fx.request(&url)
        })
        .await
        .unwrap();
    assert_eq!(fx.stored(id).mirrors, Vec::<String>::new());
    assert_eq!(wait(&engine, id).await, JobState::Completed);
    assert_file(&fx.dest.join("x.bin"), &spec);
    for path in ["/longer.bin", "/etag.bin", "/gone.bin"] {
        assert_eq!(mirror.requests(path), 1, "{path}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mirror_validators_follow_the_segment_rule_at_add() {
    let fx = Fixture::new().await;
    let engine = fx.engine().await;
    let mut spec = FileSpec::new(4 * MIB);
    spec.etag = Some("\"5f2a:12\"".into());
    spec.last_modified = Some("Tue, 01 Jan 2030 00:00:00 GMT".into());
    let url = fx.server.serve(PATH, spec.clone());
    let mirror = TestServer::start().await;
    // No ETag on the mirror: Last-Modified decides.
    let mut older = spec.clone();
    older.etag = None;
    older.last_modified = Some("Mon, 31 Dec 2029 00:00:00 GMT".into());
    // Another backend's ETag for the same bytes, same Last-Modified.
    let mut backend = spec.clone();
    backend.etag = Some("\"5f2a:13\"".into());
    let kept = mirror.serve("/backend.bin", backend);
    let mirrors = vec![mirror.serve("/older.bin", older), kept.clone()];

    let id = engine
        .add(AddRequest {
            mirrors,
            ..fx.request(&url)
        })
        .await
        .unwrap();
    assert_eq!(fx.stored(id).mirrors, [kept]);
    assert_eq!(wait(&engine, id).await, JobState::Completed);
    assert_file(&fx.dest.join("x.bin"), &spec);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn expiring_redirect_target_still_completes() {
    let fx = Fixture::new().await;
    let engine = fx.engine().await;
    // A signed link: each target serves 6 requests, then refuses with 403.
    let mut spec = FileSpec::new(8 * MIB);
    spec.fail_after = Some(fail_after(6, 403));
    fx.server.serve("/s1", spec.clone());
    fx.server.serve("/s2", spec.clone());
    let dl = fx.server.redirect_cycle("/dl", &["/s1", "/s2"]);

    // 8 connections make 1 MiB segments, too small to steal from: exactly 8
    // segment requests. Through `/dl` they alternate /s2, /s1, ... after the
    // probe's /s1, so /s1 serves 5 and /s2 4. Reusing the redirect target
    // would send all 9 requests to /s1, which refuses after 6.
    let id = engine
        .add(AddRequest {
            connections: Some(8),
            ..fx.request(&dl)
        })
        .await
        .unwrap();
    assert_eq!(wait(&engine, id).await, JobState::Completed);
    let v = view(&engine, id);
    assert_eq!(v.url, dl);
    assert_file(&fx.dest.join(&v.file_name), &spec);
    assert_eq!(fx.server.requests("/dl"), 9);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn referer_is_sent() {
    let fx = Fixture::new().await;
    let engine = fx.engine().await;
    let referer = "https://forum.example/thread/42";
    let spec = FileSpec::new(2 * MIB);
    fx.server.serve(PATH, spec.clone());
    // Through a redirect, which must keep the given Referer.
    let dl = fx.server.redirect_cycle("/dl", &[PATH]);
    let mirror = TestServer::start().await;
    let mirror_url = mirror.serve(PATH, spec.clone());

    // Two 1 MiB segments: one from the primary, one from the mirror.
    let id = engine
        .add(AddRequest {
            connections: Some(2),
            mirrors: vec![mirror_url],
            referer: Some(referer.into()),
            ..fx.request(&dl)
        })
        .await
        .unwrap();
    assert_eq!(wait(&engine, id).await, JobState::Completed);
    assert_eq!(fx.stored(id).referer.as_deref(), Some(referer));
    // Each path's last request is a segment request.
    for (server, path) in [(&fx.server, "/dl"), (&fx.server, PATH), (&mirror, PATH)] {
        assert!(server.requests(path) >= 2, "{path}: no segment request");
        let headers = server.last_request_headers(path);
        assert!(
            headers.contains(&("referer".into(), referer.into())),
            "{path}: {headers:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resume_follows_a_fresh_confirm_link() {
    let fx = Fixture::new().await;
    let engine = fx.engine().await;
    let spec = throttled(20 * MIB);
    let id = add_and_pause(&fx, &engine, &spec).await;

    // The stored link now answers with a confirm page for a new link.
    let form = r#"<form action="/y.bin"><input type="hidden" name="confirm" value="u"></form>"#;
    fx.server.serve_page(PATH, 200, &[], form);
    let fresh = fx.server.serve("/y.bin", unthrottled(&spec));
    engine.resume(id).await.unwrap();
    assert_eq!(wait(&engine, id).await, JobState::Completed);
    assert_file(&fx.dest.join("x.bin"), &spec);
    assert_eq!(view(&engine, id).url, format!("{fresh}?confirm=u"));
    assert_eq!(fx.stored(id).url, format!("{fresh}?confirm=u"));
    assert_eq!(fx.server.requests(PATH), 1);
}

/// A 1-byte answer to the probe claiming a file of `size` bytes.
fn serve_claimed_size(fx: &Fixture, path: &str, size: u64) -> String {
    let range = format!("bytes 0-0/{size}");
    fx.server.serve_page(
        path,
        206,
        &[
            ("content-type", "application/octet-stream"),
            ("content-range", &range),
        ],
        "x",
    )
}

/// Far more than any test machine's free space.
const HUGE: u64 = 1 << 60;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn add_refuses_a_file_larger_than_the_free_space() {
    let fx = Fixture::new().await;
    let engine = fx.engine().await;
    let url = serve_claimed_size(&fx, PATH, HUGE);

    let err = engine.add(fx.request(&url)).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::DiskFull);
    assert!(err.detail.contains(&HUGE.to_string()), "{err}");
    assert!(engine.snapshot().is_empty());
    assert!(!fx.tapa().exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resume_refuses_when_the_file_no_longer_fits() {
    let fx = Fixture::new().await;
    let engine = fx.engine().await;
    let id = add_and_pause(&fx, &engine, &throttled(20 * MIB)).await;

    // The file grew past the free space: it restarts, and does not fit.
    serve_claimed_size(&fx, PATH, HUGE);
    let err = engine.resume(id).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::DiskFull);
    let v = view(&engine, id);
    assert_eq!(v.state, JobState::Failed);
    assert_eq!(v.error.map(|(kind, _)| kind), Some(ErrorKind::DiskFull));
    let stored = fx.stored(id);
    assert_eq!(stored.state, JobState::Failed);
    assert_eq!(
        stored.error.map(|(kind, _)| kind),
        Some(ErrorKind::DiskFull)
    );
    assert_eq!(wait(&engine, id).await, JobState::Failed);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn add_rejects_an_invalid_checksum() {
    let fx = Fixture::new().await;
    let engine = fx.engine().await;
    let url = fx.server.serve(PATH, FileSpec::new(MIB));

    let err = engine
        .add(AddRequest {
            checksum: Some("sha256:abc".into()),
            ..fx.request(&url)
        })
        .await
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Unknown);
    assert_eq!(err.detail, "invalid checksum");
    assert!(engine.snapshot().is_empty());
    assert_eq!(fx.server.requests(PATH), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn auto_checksum_follows_the_setting() {
    let fx = Fixture::new().await;
    let engine = fx.engine().await;
    let spec = FileSpec::new(MIB);
    let url = fx.server.serve("/rel/x.iso", spec.clone());
    let wrong = "d".repeat(64);
    fx.server.serve_page(
        "/rel/SHA256SUMS",
        200,
        &[("content-type", "text/plain")],
        &format!("{wrong}  x.iso\n"),
    );

    // On by default.
    let id = engine.add(fx.request(&url)).await.unwrap();
    assert_eq!(wait(&engine, id).await, JobState::Failed);
    let (kind, detail) = view(&engine, id).error.unwrap();
    assert_eq!(kind, ErrorKind::ChecksumMismatch);
    assert!(
        detail.starts_with(&format!("expected {wrong}, got ")),
        "{detail}"
    );

    engine
        .set_settings(Settings {
            auto_checksum: false,
            ..Settings::default()
        })
        .await
        .unwrap();
    let id = engine.add(fx.request(&url)).await.unwrap();
    assert_eq!(wait(&engine, id).await, JobState::Completed);
    assert_file(&fx.dest.join(view(&engine, id).file_name), &spec);
    assert_eq!(fx.server.requests("/rel/SHA256SUMS"), 1);
}
