use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use taurus_core::store::{NewJob, Store};
use taurus_core::{
    AddRequest, Engine, EngineConfig, ErrorKind, Event, JobId, JobState, JobView, Settings,
    SpeedLimit, Timing,
};
use taurus_testkit::{FileSpec, TestServer, content, sha256_hex};
use tokio::sync::broadcast;

const MIB: usize = 1024 * 1024;
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

    fn config(&self) -> EngineConfig {
        let mut config = EngineConfig::new(self.data_dir());
        config.timing = Timing::fast_for_tests();
        config
    }

    async fn engine(&self) -> Engine {
        Engine::open(self.config()).await.unwrap()
    }

    /// An engine that runs at most `max_active` jobs at a time.
    async fn engine_with_max(&self, max_active: u32) -> Engine {
        let engine = self.engine().await;
        engine
            .set_settings(Settings {
                max_active,
                ..Settings::default()
            })
            .await
            .unwrap();
        engine
    }

    fn request(&self, url: &str) -> AddRequest {
        AddRequest {
            url: url.to_string(),
            dest_dir: self.dest.clone(),
            ..AddRequest::default()
        }
    }

    /// Serves `spec` at `/<name>` and adds it.
    async fn add(&self, engine: &Engine, name: &str, spec: &FileSpec) -> JobId {
        let url = self.server.serve(&format!("/{name}"), spec.clone());
        engine.add(self.request(&url)).await.unwrap()
    }
}

/// Capped at `bps` for its own path.
fn throttled(size: usize, bps: usize) -> FileSpec {
    let mut spec = FileSpec::new(size);
    spec.bytes_per_sec = Some(bps as u64);
    spec
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

/// Events until (and including) the first one `stop` accepts.
async fn events_until(
    events: &mut broadcast::Receiver<Event>,
    stop: impl Fn(&Event) -> bool,
) -> Vec<Event> {
    let mut seen = Vec::new();
    loop {
        let event = next_event(events).await;
        let done = stop(&event);
        seen.push(event);
        if done {
            return seen;
        }
    }
}

fn view(engine: &Engine, id: JobId) -> JobView {
    engine
        .snapshot()
        .into_iter()
        .find(|v| v.id == id)
        .expect("job is in the snapshot")
}

fn downloading(engine: &Engine) -> usize {
    engine
        .snapshot()
        .iter()
        .filter(|v| v.state == JobState::Downloading)
        .count()
}

fn queue_idles(events: &[Event]) -> usize {
    events
        .iter()
        .filter(|e| matches!(e, Event::QueueIdle))
        .count()
}

fn is_update(event: &Event, id: JobId, state: JobState) -> bool {
    matches!(event, Event::JobUpdated(v) if v.id == id && v.state == state)
}

/// Every event already sent.
fn drain(events: &mut broadcast::Receiver<Event>) -> Vec<Event> {
    let mut seen = Vec::new();
    while let Ok(event) = events.try_recv() {
        seen.push(event);
    }
    seen
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn max_active_limits_concurrency() {
    let fx = Fixture::new().await;
    let engine = fx.engine_with_max(2).await;
    let mut events = engine.subscribe();
    let spec = throttled(4 * MIB, 2 * MIB);
    let mut ids = Vec::new();
    for name in ["a.bin", "b.bin", "c.bin", "d.bin"] {
        ids.push(fx.add(&engine, name, &spec).await);
    }

    let sampler = {
        let engine = engine.clone();
        tokio::spawn(async move {
            let mut most = 0;
            loop {
                let snapshot = engine.snapshot();
                let active = snapshot
                    .iter()
                    .filter(|v| v.state == JobState::Downloading)
                    .count();
                most = most.max(active);
                if snapshot.iter().all(|v| v.state == JobState::Completed) {
                    return most;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
    };
    let mut seen = events_until(&mut events, |e| matches!(e, Event::QueueIdle)).await;
    let most = tokio::time::timeout(TIMEOUT, sampler)
        .await
        .unwrap()
        .unwrap();
    assert!(most <= 2, "{most} jobs downloaded at once");
    assert_eq!(most, 2, "the limit was never reached");
    for id in &ids {
        assert_eq!(wait(&engine, *id).await, JobState::Completed);
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    seen.extend(drain(&mut events));
    assert_eq!(queue_idles(&seen), 1, "{seen:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn start_now_bypasses_limit() {
    let fx = Fixture::new().await;
    let engine = fx.engine_with_max(1).await;
    let spec = throttled(20 * MIB, 4 * MIB);
    let first = fx.add(&engine, "a.bin", &spec).await;
    let second = fx.add(&engine, "b.bin", &spec).await;
    assert_eq!(view(&engine, first).state, JobState::Downloading);
    assert_eq!(view(&engine, second).state, JobState::Queued);

    engine.start_now(second).await.unwrap();
    assert_eq!(view(&engine, second).state, JobState::Downloading);
    assert_eq!(downloading(&engine), 2);
    engine.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn queue_order_is_fifo() {
    let fx = Fixture::new().await;
    let engine = fx.engine_with_max(1).await;
    let mut events = engine.subscribe();
    // Slow enough that A still runs while B and C are added.
    let spec = throttled(2 * MIB, 4 * MIB);
    let mut ids = Vec::new();
    for name in ["a.bin", "b.bin", "c.bin"] {
        ids.push(fx.add(&engine, name, &spec).await);
    }

    let seen = events_until(&mut events, |e| is_update(e, ids[2], JobState::Completed)).await;
    let started: Vec<JobId> = seen
        .iter()
        .filter_map(|e| match e {
            Event::JobUpdated(v) if v.state == JobState::Downloading => Some(v.id),
            _ => None,
        })
        .collect();
    assert_eq!(started, ids);
    for id in ids {
        assert_eq!(wait(&engine, id).await, JobState::Completed);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transient_failure_waits_then_recovers() {
    let fx = Fixture::new().await;
    let engine = fx.engine().await;
    let mut events = engine.subscribe();
    let mut spec = FileSpec::new(MIB);
    // The add-time probe passes; the job's requests then get 503s.
    spec.fail_first = [vec![0], vec![503; 6]].concat();
    let id = fx.add(&engine, "x.bin", &spec).await;

    let seen = events_until(&mut events, |e| is_update(e, id, JobState::Completed)).await;
    let waiting = seen.iter().find_map(|e| match e {
        Event::JobUpdated(v) if v.id == id && v.state == JobState::Waiting => Some(v.clone()),
        _ => None,
    });
    let waiting = waiting.expect("the job never waited");
    assert_eq!(
        waiting.error.map(|(kind, _)| kind),
        Some(ErrorKind::RateLimited)
    );
    assert_eq!(wait(&engine, id).await, JobState::Completed);
    let got = fs::read(fx.dest.join("x.bin")).unwrap();
    assert_eq!(sha256_hex(&got), sha256_hex(&content(&spec)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pause_cancels_waiting_retry() {
    let fx = Fixture::new().await;
    let engine = fx.engine().await;
    let mut events = engine.subscribe();
    let mut spec = FileSpec::new(MIB);
    spec.fail_first = [vec![0], vec![503; 1000]].concat();
    let id = fx.add(&engine, "x.bin", &spec).await;
    events_until(&mut events, |e| is_update(e, id, JobState::Waiting)).await;

    engine.pause(id).await.unwrap();
    assert_eq!(view(&engine, id).state, JobState::Paused);
    assert_eq!(wait(&engine, id).await, JobState::Paused);
    let requests = fx.server.requests("/x.bin");
    tokio::time::sleep(Timing::fast_for_tests().waiting_retry_cap * 2).await;
    assert_eq!(view(&engine, id).state, JobState::Paused);
    assert_eq!(
        fx.server.requests("/x.bin"),
        requests,
        "retried while paused"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn raising_max_active_starts_more() {
    let fx = Fixture::new().await;
    let engine = fx.engine_with_max(1).await;
    let spec = throttled(20 * MIB, 4 * MIB);
    for name in ["a.bin", "b.bin", "c.bin"] {
        fx.add(&engine, name, &spec).await;
    }
    assert_eq!(downloading(&engine), 1);

    engine
        .set_settings(Settings {
            max_active: 3,
            ..Settings::default()
        })
        .await
        .unwrap();
    assert_eq!(downloading(&engine), 3);
    engine.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn settings_persist_across_reopen() {
    let fx = Fixture::new().await;
    let settings = Settings {
        max_active: 5,
        speed: SpeedLimit::Custom {
            bytes_per_sec: 123_456,
        },
        auto_checksum: false,
        ..Settings::default()
    };
    {
        let engine = fx.engine().await;
        engine.set_settings(settings.clone()).await.unwrap();
        engine.shutdown().await.unwrap();
    }
    let engine = fx.engine().await;
    assert_eq!(engine.settings(), settings);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lowering_max_active_lets_running_jobs_finish() {
    let fx = Fixture::new().await;
    let engine = fx.engine_with_max(2).await;
    let spec = throttled(3 * MIB, 2 * MIB);
    let a = fx.add(&engine, "a.bin", &spec).await;
    let b = fx.add(&engine, "b.bin", &spec).await;
    let c = fx.add(&engine, "c.bin", &spec).await;
    assert_eq!(downloading(&engine), 2);

    engine
        .set_settings(Settings {
            max_active: 1,
            ..Settings::default()
        })
        .await
        .unwrap();
    assert_eq!(downloading(&engine), 2);
    assert_eq!(view(&engine, c).state, JobState::Queued);
    for id in [a, b, c] {
        assert_eq!(wait(&engine, id).await, JobState::Completed);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn paused_and_failed_jobs_do_not_block_queue_idle() {
    let fx = Fixture::new().await;
    let engine = fx.engine_with_max(3).await;
    let mut events = engine.subscribe();
    let slow = fx
        .add(&engine, "slow.bin", &throttled(20 * MIB, 4 * MIB))
        .await;
    let mut gone = FileSpec::new(MIB);
    gone.fail_first = vec![0, 410, 410, 410, 410];
    let failed = fx.add(&engine, "gone.bin", &gone).await;
    let done = fx.add(&engine, "ok.bin", &FileSpec::new(MIB)).await;
    assert_eq!(wait(&engine, failed).await, JobState::Failed);
    assert_eq!(wait(&engine, done).await, JobState::Completed);
    assert_eq!(queue_idles(&drain(&mut events)), 0);

    engine.pause(slow).await.unwrap();
    let seen = events_until(&mut events, |e| matches!(e, Event::QueueIdle)).await;
    assert!(seen.iter().any(|e| is_update(e, slow, JobState::Paused)));
    // Pausing an idle queue again announces nothing.
    engine.pause(slow).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(queue_idles(&drain(&mut events)), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pausing_a_queued_job_holds_it_and_resume_requeues_it() {
    let fx = Fixture::new().await;
    let engine = fx.engine_with_max(1).await;
    let first = fx.add(&engine, "a.bin", &throttled(4 * MIB, 4 * MIB)).await;
    let second = fx.add(&engine, "b.bin", &FileSpec::new(MIB)).await;
    assert_eq!(view(&engine, second).state, JobState::Queued);

    engine.pause(second).await.unwrap();
    assert_eq!(view(&engine, second).state, JobState::Paused);
    assert_eq!(wait(&engine, second).await, JobState::Paused);
    assert_eq!(wait(&engine, first).await, JobState::Completed);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(view(&engine, second).state, JobState::Paused);

    engine.resume(second).await.unwrap();
    assert_eq!(wait(&engine, second).await, JobState::Completed);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn open_keeps_queued_and_waiting_jobs_and_starts_them() {
    let fx = Fixture::new().await;
    let spec = FileSpec::new(MIB);
    let mut ids = Vec::new();
    fs::create_dir_all(fx.data_dir()).unwrap();
    {
        let store = Store::open(&fx.data_dir().join("taurus.db")).unwrap();
        for (name, state) in [("a.bin", JobState::Queued), ("b.bin", JobState::Waiting)] {
            let url = fx.server.serve(&format!("/{name}"), spec.clone());
            let id = store
                .insert_job(&NewJob {
                    url: url.clone(),
                    dest_dir: fx.dest.clone(),
                    file_name: name.into(),
                    connections: 4,
                    mirrors: Vec::new(),
                    checksum: None,
                    referer: None,
                    queue_pos: 0,
                })
                .unwrap();
            let mut job = store.get_job(id).unwrap().unwrap();
            job.state = state;
            job.final_url = Some(url);
            job.total = Some(spec.size as u64);
            job.resumable = true;
            if state == JobState::Waiting {
                job.error = Some((ErrorKind::ConnectionLost, "offline".into()));
            }
            store.update_job(&job).unwrap();
            ids.push(id);
        }
    }

    let engine = fx.engine().await;
    for id in ids {
        assert_eq!(wait(&engine, id).await, JobState::Completed);
        assert_eq!(view(&engine, id).error, None);
    }
    for name in ["a.bin", "b.bin"] {
        let got = fs::read(fx.dest.join(name)).unwrap();
        assert_eq!(sha256_hex(&got), sha256_hex(&content(&spec)));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn remove_cancels_waiting_retry() {
    let fx = Fixture::new().await;
    let engine = fx.engine().await;
    let mut events = engine.subscribe();
    let mut spec = FileSpec::new(MIB);
    spec.fail_first = [vec![0], vec![503; 1000]].concat();
    let id = fx.add(&engine, "x.bin", &spec).await;
    events_until(&mut events, |e| is_update(e, id, JobState::Waiting)).await;

    engine.remove(id, true).await.unwrap();
    let requests = fx.server.requests("/x.bin");
    tokio::time::sleep(Timing::fast_for_tests().waiting_retry_cap * 2).await;
    assert!(engine.snapshot().is_empty());
    assert_eq!(
        fx.server.requests("/x.bin"),
        requests,
        "retried after remove"
    );
}

/// Jobs that wait in the queue have no partial file yet: only the recorded
/// names keep them apart.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn queued_jobs_in_an_aliased_folder_get_distinct_names() {
    let fx = Fixture::new().await;
    let engine = fx.engine_with_max(1).await;
    fx.add(&engine, "busy.bin", &throttled(20 * MIB, 4 * MIB))
        .await;
    let url = fx.server.serve("/x.bin", FileSpec::new(MIB));
    let a = engine.add(fx.request(&url)).await.unwrap();
    let mut aliased = fx.request(&url);
    aliased.dest_dir = fx.dest.join(".");
    let b = engine.add(aliased).await.unwrap();
    assert_eq!(view(&engine, b).state, JobState::Queued);

    let names: HashSet<String> = [a, b]
        .iter()
        .map(|id| view(&engine, *id).file_name)
        .collect();
    let expected: HashSet<String> = ["x.bin".to_string(), "x (1).bin".to_string()].into();
    assert_eq!(names, expected);
    assert_eq!(view(&engine, a).dest_dir, view(&engine, b).dest_dir);
    engine.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn queued_names_differing_only_in_case_are_kept_apart() {
    let fx = Fixture::new().await;
    let engine = fx.engine_with_max(1).await;
    fx.add(&engine, "busy.bin", &throttled(20 * MIB, 4 * MIB))
        .await;
    let url = fx.server.serve("/x.bin", FileSpec::new(MIB));
    let mut upper = fx.request(&url);
    upper.file_name = Some("A.bin".into());
    let a = engine.add(upper).await.unwrap();
    let mut lower = fx.request(&url);
    lower.file_name = Some("a.bin".into());
    let b = engine.add(lower).await.unwrap();

    assert_eq!(view(&engine, a).file_name, "A.bin");
    assert_eq!(view(&engine, b).file_name, "a (1).bin");
    engine.shutdown().await.unwrap();
}
