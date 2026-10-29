use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use taurus_core::store::{NewJob, Store};
use taurus_core::system::command_for;
use taurus_core::system::space::SPACE_MARGIN;
use taurus_core::{
    AddRequest, Engine, EngineConfig, ErrorKind, Event, JobId, JobState, JobView, PowerMode,
    Settings, SpeedLimit, Timing, WhenDone,
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

    /// Fast timing; sleep and shutdown never run for real.
    fn config(&self) -> EngineConfig {
        let mut config = EngineConfig::new(self.data_dir());
        config.timing = Timing::fast_for_tests();
        config.power = PowerMode::DryRun;
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

    /// An engine whose when-done countdown lasts `countdown`, with `settings`.
    async fn engine_with(&self, countdown: Duration, settings: Settings) -> Engine {
        let mut config = self.config();
        config.timing.when_done_countdown = countdown;
        let engine = Engine::open(config).await.unwrap();
        engine.set_settings(settings).await.unwrap();
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

/// Records a job per `(name, state)` straight in the database, as an
/// earlier engine would have left it, each serving `spec` at `/<name>`.
fn seed_jobs(fx: &Fixture, spec: &FileSpec, jobs: &[(&str, JobState)]) -> Vec<JobId> {
    fs::create_dir_all(fx.data_dir()).unwrap();
    let store = Store::open(&fx.data_dir().join("taurus.db")).unwrap();
    jobs.iter()
        .map(|&(name, state)| {
            let url = fx.server.serve(&format!("/{name}"), spec.clone());
            let id = store
                .insert_job(&NewJob {
                    url: url.clone(),
                    source_url: url.clone(),
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
            id
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn open_keeps_queued_and_waiting_jobs_and_starts_them() {
    let fx = Fixture::new().await;
    let spec = FileSpec::new(MIB);
    let ids = seed_jobs(
        &fx,
        &spec,
        &[("a.bin", JobState::Queued), ("b.bin", JobState::Waiting)],
    );

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
async fn without_autostart_only_start_now_runs_jobs() {
    let fx = Fixture::new().await;
    let spec = FileSpec::new(MIB);
    let seeded = seed_jobs(
        &fx,
        &spec,
        &[("a.bin", JobState::Queued), ("b.bin", JobState::Waiting)],
    );
    let mut config = fx.config();
    config.autostart = false;
    let engine = Engine::open(config).await.unwrap();

    // Nothing starts on open, on add, or on any other change.
    let added = fx.add(&engine, "c.bin", &spec).await;
    let probes = fx.server.requests("/c.bin");
    engine
        .set_settings(Settings {
            max_active: 5,
            ..Settings::default()
        })
        .await
        .unwrap();
    tokio::time::sleep(Timing::fast_for_tests().waiting_retry_cap * 2).await;
    let states: Vec<JobState> = engine.snapshot().iter().map(|v| v.state).collect();
    assert_eq!(
        states,
        [JobState::Queued, JobState::Waiting, JobState::Queued]
    );
    assert_eq!(fx.server.requests("/a.bin"), 0);
    assert_eq!(fx.server.requests("/b.bin"), 0);
    assert_eq!(fx.server.requests("/c.bin"), probes);
    assert!(!fx.dest.join("a.bin.tapa").exists());

    engine.start_now(added).await.unwrap();
    assert_eq!(wait(&engine, added).await, JobState::Completed);
    assert_eq!(view(&engine, seeded[0]).state, JobState::Queued);
    engine.shutdown().await.unwrap();

    let store = Store::open(&fx.data_dir().join("taurus.db")).unwrap();
    let states: Vec<JobState> = store.list_jobs().unwrap().iter().map(|j| j.state).collect();
    assert_eq!(
        states,
        [JobState::Queued, JobState::Waiting, JobState::Completed]
    );
    assert_eq!(fx.server.requests("/a.bin"), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn without_autostart_an_interrupted_job_reopens_queued() {
    let fx = Fixture::new().await;
    let ids = seed_jobs(
        &fx,
        &FileSpec::new(MIB),
        &[("a.bin", JobState::Downloading)],
    );
    let mut config = fx.config();
    config.autostart = false;
    let engine = Engine::open(config).await.unwrap();

    assert_eq!(view(&engine, ids[0]).state, JobState::Queued);
    tokio::time::sleep(Timing::fast_for_tests().waiting_retry_cap * 2).await;
    assert_eq!(view(&engine, ids[0]).state, JobState::Queued);
    assert_eq!(fx.server.requests("/a.bin"), 0);
    engine.shutdown().await.unwrap();

    let store = Store::open(&fx.data_dir().join("taurus.db")).unwrap();
    assert_eq!(
        store.get_job(ids[0]).unwrap().unwrap().state,
        JobState::Queued
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_interrupted_job_restarts_before_later_queued_ones() {
    let fx = Fixture::new().await;
    fx.engine_with_max(1).await.shutdown().await.unwrap();
    // About 1 s each.
    let spec = throttled(2 * MIB, 2 * MIB);
    let ids = seed_jobs(
        &fx,
        &spec,
        &[
            ("a.bin", JobState::Downloading),
            ("b.bin", JobState::Queued),
        ],
    );

    let engine = fx.engine().await;
    assert_eq!(view(&engine, ids[0]).state, JobState::Downloading);
    assert_eq!(view(&engine, ids[1]).state, JobState::Queued);
    assert_eq!(wait(&engine, ids[0]).await, JobState::Completed);
    assert_eq!(wait(&engine, ids[1]).await, JobState::Completed);
    for name in ["a.bin", "b.bin"] {
        let got = fs::read(fx.dest.join(name)).unwrap();
        assert_eq!(sha256_hex(&got), sha256_hex(&content(&spec)));
    }
    engine.shutdown().await.unwrap();
}

const BIG: usize = 2 * MIB;
const SMALL: usize = MIB / 2;

/// Free space for three big files and a small one, whatever the real disk has.
fn room_for_three_and_a_small(_dir: &std::path::Path) -> std::io::Result<u64> {
    Ok(3 * BIG as u64 + SMALL as u64 + SPACE_MARGIN)
}

fn no_room(_dir: &std::path::Path) -> std::io::Result<u64> {
    Ok(0)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_queued_job_that_no_longer_fits_fails_as_it_would_start() {
    let fx = Fixture::new().await;
    fx.engine_with_max(5).await.shutdown().await.unwrap();
    // Each fits on its own; together they do not.
    let big = FileSpec::new(BIG);
    let small = FileSpec::new(SMALL);
    let mut ids = seed_jobs(
        &fx,
        &big,
        &[
            ("a.bin", JobState::Queued),
            ("b.bin", JobState::Queued),
            ("c.bin", JobState::Queued),
            ("d.bin", JobState::Queued),
        ],
    );
    ids.extend(seed_jobs(&fx, &small, &[("e.bin", JobState::Queued)]));
    let mut config = fx.config();
    config.available_space = room_for_three_and_a_small;
    let engine = Engine::open(config).await.unwrap();

    // d needs what a, b and c still download; e, after it, still fits.
    let d = view(&engine, ids[3]);
    assert_eq!(d.state, JobState::Failed);
    let (kind, detail) = d.error.unwrap();
    assert_eq!(kind, ErrorKind::DiskFull);
    assert!(detail.contains(&(3 * BIG).to_string()), "{detail}");
    for (i, id) in ids.iter().enumerate().filter(|&(i, _)| i != 3) {
        assert_eq!(wait(&engine, *id).await, JobState::Completed, "job {i}");
    }
    assert_eq!(wait(&engine, ids[3]).await, JobState::Failed);
    assert_eq!(fx.server.requests("/d.bin"), 0);
    engine.shutdown().await.unwrap();

    let store = Store::open(&fx.data_dir().join("taurus.db")).unwrap();
    let stored = store.get_job(ids[3]).unwrap().unwrap();
    assert_eq!(stored.state, JobState::Failed);
    assert_eq!(
        stored.error.map(|(kind, _)| kind),
        Some(ErrorKind::DiskFull)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn start_now_checks_free_space() {
    let fx = Fixture::new().await;
    let ids = seed_jobs(&fx, &FileSpec::new(MIB), &[("a.bin", JobState::Queued)]);
    let mut config = fx.config();
    config.autostart = false;
    config.available_space = no_room;
    let engine = Engine::open(config).await.unwrap();
    let mut events = engine.subscribe();

    let err = engine.start_now(ids[0]).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::DiskFull);
    let v = view(&engine, ids[0]);
    assert_eq!(v.state, JobState::Failed);
    assert_eq!(v.error.map(|(kind, _)| kind), Some(ErrorKind::DiskFull));
    assert!(
        drain(&mut events)
            .iter()
            .any(|e| is_update(e, ids[0], JobState::Failed))
    );
    assert_eq!(fx.server.requests("/a.bin"), 0);
    engine.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_job_with_nothing_left_to_download_starts_without_free_space() {
    let fx = Fixture::new().await;
    let spec = FileSpec::new(MIB);
    // Stopped while hashing: every byte is down, only the check and the
    // rename are left.
    let ids = seed_jobs(&fx, &spec, &[("a.bin", JobState::Queued)]);
    fs::write(fx.dest.join("a.bin.tapa"), content(&spec)).unwrap();
    let whole = taurus_core::plan::Segment {
        start: 0,
        end: spec.size as u64,
        done: spec.size as u64,
    };
    Store::open(&fx.data_dir().join("taurus.db"))
        .unwrap()
        .replace_segments(ids[0], &[whole])
        .unwrap();
    let mut config = fx.config();
    config.available_space = no_room;
    let engine = Engine::open(config).await.unwrap();

    assert_eq!(wait(&engine, ids[0]).await, JobState::Completed);
    let got = fs::read(fx.dest.join("a.bin")).unwrap();
    assert_eq!(sha256_hex(&got), sha256_hex(&content(&spec)));
    engine.shutdown().await.unwrap();
}

/// Turns the seeded job into one a crash interrupted halfway through
/// `spec`: Downloading, with `spec`'s validators and its first half on disk.
fn interrupt_halfway(fx: &Fixture, id: JobId, name: &str, spec: &FileSpec) {
    let store = Store::open(&fx.data_dir().join("taurus.db")).unwrap();
    let mut job = store.get_job(id).unwrap().unwrap();
    job.state = JobState::Downloading;
    job.etag = spec.etag.clone();
    store.update_job(&job).unwrap();
    let half = spec.size as u64 / 2;
    let data = content(spec);
    fs::write(fx.dest.join(format!("{name}.tapa")), &data[..half as usize]).unwrap();
    let whole = taurus_core::plan::Segment {
        start: 0,
        end: spec.size as u64,
        done: half,
    };
    store.replace_segments(id, &[whole]).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_job_interrupted_by_a_crash_restarts_from_zero_if_the_file_changed() {
    let fx = Fixture::new().await;
    let mut old = FileSpec::new(2 * MIB);
    old.etag = Some("\"v1\"".into());
    let ids = seed_jobs(&fx, &old, &[("a.bin", JobState::Queued)]);
    interrupt_halfway(&fx, ids[0], "a.bin", &old);
    let mut changed = old.clone();
    changed.seed = 2;
    changed.etag = Some("\"v2\"".into());
    fx.server.replace("/a.bin", changed.clone());

    let engine = fx.engine().await;
    assert_eq!(wait(&engine, ids[0]).await, JobState::Completed);
    let got = fs::read(fx.dest.join("a.bin")).unwrap();
    assert_eq!(sha256_hex(&got), sha256_hex(&content(&changed)));
    engine.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_interrupted_job_whose_check_fails_for_now_waits() {
    let fx = Fixture::new().await;
    let mut spec = FileSpec::new(2 * MIB);
    // The check before the restart is refused once.
    spec.fail_first = vec![503];
    let ids = seed_jobs(&fx, &spec, &[("a.bin", JobState::Queued)]);
    interrupt_halfway(&fx, ids[0], "a.bin", &spec);
    let mut config = fx.config();
    config.autostart = false;
    let engine = Engine::open(config).await.unwrap();
    let mut events = engine.subscribe();

    engine.start_now(ids[0]).await.unwrap();
    let seen = events_until(&mut events, |e| {
        is_update(e, ids[0], JobState::Waiting) || is_update(e, ids[0], JobState::Completed)
    })
    .await;
    assert!(is_update(seen.last().unwrap(), ids[0], JobState::Waiting));
    let v = view(&engine, ids[0]);
    assert_eq!(v.error.map(|(kind, _)| kind), Some(ErrorKind::RateLimited));
    assert_eq!(fx.server.requests("/a.bin"), 1);

    // Still checked first when started again; then it goes on from its half.
    engine.start_now(ids[0]).await.unwrap();
    assert_eq!(wait(&engine, ids[0]).await, JobState::Completed);
    let got = fs::read(fx.dest.join("a.bin")).unwrap();
    assert_eq!(sha256_hex(&got), sha256_hex(&content(&spec)));
    assert!(fx.server.bytes_served("/a.bin") < spec.size as u64);
    engine.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn max_active_zero_is_rejected() {
    let fx = Fixture::new().await;
    let engine = fx.engine_with_max(2).await;
    let mut events = engine.subscribe();
    let err = engine
        .set_settings(Settings {
            max_active: 0,
            ..Settings::default()
        })
        .await
        .unwrap_err();
    assert_eq!(
        (err.kind, err.detail.as_str()),
        (ErrorKind::Unknown, "max_active must be at least 1")
    );
    assert_eq!(engine.settings().max_active, 2);
    assert!(drain(&mut events).is_empty());
    engine.shutdown().await.unwrap();
    assert_eq!(fx.engine().await.settings().max_active, 2);
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

fn when_done(action: WhenDone) -> Settings {
    Settings {
        when_done: action,
        ..Settings::default()
    }
}

/// `QueueIdle` and the when-done events, in order.
fn when_done_events(events: &[Event]) -> Vec<Event> {
    events
        .iter()
        .filter(|e| {
            matches!(
                e,
                Event::QueueIdle
                    | Event::WhenDoneCountdown { .. }
                    | Event::WhenDoneCancelled
                    | Event::WhenDoneFired { .. }
            )
        })
        .cloned()
        .collect()
}

fn is_fired(event: &Event) -> bool {
    matches!(event, Event::WhenDoneFired { .. })
}

fn is_countdown(event: &Event) -> bool {
    matches!(event, Event::WhenDoneCountdown { .. })
}

fn cancellations(events: &[Event]) -> usize {
    events
        .iter()
        .filter(|e| matches!(e, Event::WhenDoneCancelled))
        .count()
}

/// The command line `action` runs on this OS.
fn command_line(action: WhenDone) -> Option<String> {
    command_for(action).map(|(program, args)| [vec![program], args].concat().join(" "))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sleep_fires_after_countdown() {
    let fx = Fixture::new().await;
    let countdown = Timing::fast_for_tests().when_done_countdown;
    let engine = fx.engine_with(countdown, when_done(WhenDone::Sleep)).await;
    let mut events = engine.subscribe();
    let started = Instant::now();
    let id = fx.add(&engine, "a.bin", &FileSpec::new(MIB)).await;

    let seen = when_done_events(&events_until(&mut events, is_fired).await);
    assert!(
        started.elapsed() >= countdown,
        "fired before the countdown ran out"
    );
    assert_eq!(wait(&engine, id).await, JobState::Completed);
    let (first, rest) = seen.split_first().unwrap();
    let (last, middle) = rest.split_last().unwrap();
    assert!(matches!(first, Event::QueueIdle), "{seen:?}");
    assert!(!middle.is_empty(), "no countdown event: {seen:?}");
    assert!(
        middle.iter().all(|e| matches!(
            e,
            Event::WhenDoneCountdown {
                action: WhenDone::Sleep,
                ..
            }
        )),
        "{seen:?}"
    );
    assert!(matches!(
        middle[0],
        Event::WhenDoneCountdown {
            seconds_left: 1,
            ..
        }
    ));
    let Event::WhenDoneFired { action, command } = last else {
        unreachable!()
    };
    assert_eq!(*action, WhenDone::Sleep);
    assert_eq!(*command, command_line(WhenDone::Sleep));
    #[cfg(target_os = "linux")]
    assert_eq!(command.as_deref(), Some("systemctl suspend"));
    engine.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn notify_fires_immediately() {
    let fx = Fixture::new().await;
    let engine = fx
        .engine_with(Duration::from_secs(30), when_done(WhenDone::Notify))
        .await;
    let mut events = engine.subscribe();
    let id = fx.add(&engine, "a.bin", &FileSpec::new(MIB)).await;

    let seen = when_done_events(&events_until(&mut events, is_fired).await);
    assert_eq!(wait(&engine, id).await, JobState::Completed);
    assert_eq!(seen.len(), 2, "{seen:?}");
    assert!(matches!(seen[0], Event::QueueIdle));
    assert!(matches!(
        seen[1],
        Event::WhenDoneFired {
            action: WhenDone::Notify,
            command: None
        }
    ));
    engine.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn adding_during_countdown_cancels() {
    let fx = Fixture::new().await;
    let engine = fx
        .engine_with(Duration::from_secs(30), when_done(WhenDone::Shutdown))
        .await;
    let mut events = engine.subscribe();
    let first = fx.add(&engine, "a.bin", &FileSpec::new(MIB)).await;
    let mut seen = events_until(&mut events, is_countdown).await;
    assert_eq!(wait(&engine, first).await, JobState::Completed);

    let second = fx.add(&engine, "b.bin", &FileSpec::new(MIB)).await;
    seen.extend(events_until(&mut events, |e| matches!(e, Event::WhenDoneCancelled)).await);
    assert_eq!(wait(&engine, second).await, JobState::Completed);
    // The queue is idle again: a new countdown starts.
    seen.extend(events_until(&mut events, is_countdown).await);
    let seen = when_done_events(&seen);
    assert!(
        matches!(
            seen.as_slice(),
            [
                Event::QueueIdle,
                Event::WhenDoneCountdown { .. },
                Event::WhenDoneCancelled,
                Event::QueueIdle,
                Event::WhenDoneCountdown { .. },
            ]
        ),
        "{seen:?}"
    );
    engine.shutdown().await.unwrap();
    assert!(!drain(&mut events).iter().any(is_fired));
}

/// A web page that takes about 2 s to arrive: `add` fails on it, late.
fn slow_page() -> FileSpec {
    let mut spec = FileSpec::new(40 * 1024);
    spec.content_type = Some("text/html".into());
    spec.bytes_per_sec = Some(16 * 1024);
    spec
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn adding_cancels_the_countdown_before_probing() {
    let fx = Fixture::new().await;
    let countdown = Duration::from_secs(1);
    let engine = fx
        .engine_with(countdown, when_done(WhenDone::Shutdown))
        .await;
    let mut events = engine.subscribe();
    fx.add(&engine, "a.bin", &FileSpec::new(MIB)).await;
    events_until(&mut events, is_countdown).await;

    // Resolving takes longer than the countdown has left.
    let url = fx.server.serve("/slow", slow_page());
    let adding = {
        let engine = engine.clone();
        let request = fx.request(&url);
        tokio::spawn(async move { engine.add(request).await })
    };
    let seen = events_until(&mut events, |e| {
        matches!(e, Event::WhenDoneCancelled) || is_fired(e)
    })
    .await;
    assert!(
        matches!(seen.last(), Some(Event::WhenDoneCancelled)),
        "{seen:?}"
    );
    assert!(
        !adding.is_finished(),
        "cancelled only once the add was done"
    );
    assert!(adding.await.unwrap().is_err());
    tokio::time::sleep(countdown * 2).await;
    let seen = drain(&mut events);
    assert!(
        !seen.iter().any(|e| is_fired(e) || is_countdown(e)),
        "{seen:?}"
    );
    engine.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_hold_defers_when_done_for_a_batch() {
    for action in [WhenDone::Notify, WhenDone::Sleep] {
        let fx = Fixture::new().await;
        let countdown = Timing::fast_for_tests().when_done_countdown;
        let engine = fx.engine_with(countdown, when_done(action)).await;
        let mut events = engine.subscribe();
        let hold = engine.hold_when_done();
        let id = fx.add(&engine, "a.bin", &FileSpec::new(MIB)).await;
        assert_eq!(wait(&engine, id).await, JobState::Completed);
        // The rest of the batch still resolves.
        let url = fx.server.serve("/slow", slow_page());
        assert!(engine.add(fx.request(&url)).await.is_err());
        let seen = when_done_events(&drain(&mut events));
        assert_eq!(seen.len(), 1, "{action:?}: {seen:?}");
        assert!(matches!(seen[0], Event::QueueIdle));

        drop(hold);
        events_until(&mut events, |e| match action {
            WhenDone::Sleep => is_countdown(e),
            _ => is_fired(e),
        })
        .await;
        engine.shutdown().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancel_when_done_stops_countdown() {
    let fx = Fixture::new().await;
    let countdown = Duration::from_secs(1);
    let engine = fx.engine_with(countdown, when_done(WhenDone::Sleep)).await;
    let mut events = engine.subscribe();
    fx.add(&engine, "a.bin", &FileSpec::new(MIB)).await;
    events_until(&mut events, is_countdown).await;

    engine.cancel_when_done().await.unwrap();
    // Cancelling when nothing counts down announces nothing.
    engine.cancel_when_done().await.unwrap();
    tokio::time::sleep(countdown * 2).await;
    let seen = drain(&mut events);
    assert_eq!(cancellations(&seen), 1, "{seen:?}");
    assert!(!seen.iter().any(is_fired), "{seen:?}");
    engine.shutdown().await.unwrap();
    assert!(engine.cancel_when_done().await.is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn changing_when_done_cancels_countdown() {
    let fx = Fixture::new().await;
    let countdown = Duration::from_secs(1);
    let engine = fx.engine_with(countdown, when_done(WhenDone::Sleep)).await;
    let mut events = engine.subscribe();
    fx.add(&engine, "a.bin", &FileSpec::new(MIB)).await;
    events_until(&mut events, is_countdown).await;

    // Another setting leaves the countdown running.
    engine
        .set_settings(Settings {
            max_active: 2,
            ..when_done(WhenDone::Sleep)
        })
        .await
        .unwrap();
    assert_eq!(cancellations(&drain(&mut events)), 0);
    engine
        .set_settings(when_done(WhenDone::Shutdown))
        .await
        .unwrap();
    tokio::time::sleep(countdown * 2).await;
    let seen = drain(&mut events);
    assert_eq!(cancellations(&seen), 1, "{seen:?}");
    assert!(!seen.iter().any(is_fired), "{seen:?}");
    engine.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn paused_jobs_do_not_block_when_done() {
    let fx = Fixture::new().await;
    let countdown = Timing::fast_for_tests().when_done_countdown;
    let engine = fx.engine_with(countdown, when_done(WhenDone::Sleep)).await;
    let mut events = engine.subscribe();
    let slow = fx
        .add(&engine, "slow.bin", &throttled(20 * MIB, 4 * MIB))
        .await;
    let mut gone = FileSpec::new(MIB);
    gone.fail_first = vec![0, 410, 410, 410, 410];
    let failed = fx.add(&engine, "gone.bin", &gone).await;
    let done = fx
        .add(&engine, "ok.bin", &throttled(2 * MIB, 4 * MIB))
        .await;
    engine.pause(slow).await.unwrap();

    let seen = events_until(&mut events, is_fired).await;
    assert_eq!(view(&engine, slow).state, JobState::Paused);
    assert_eq!(view(&engine, failed).state, JobState::Failed);
    assert_eq!(view(&engine, done).state, JobState::Completed);
    assert!(matches!(
        seen.last(),
        Some(Event::WhenDoneFired {
            action: WhenDone::Sleep,
            ..
        })
    ));
    engine.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn idling_by_pause_or_remove_runs_no_when_done() {
    for action in [WhenDone::Sleep, WhenDone::Notify] {
        let fx = Fixture::new().await;
        let countdown = Timing::fast_for_tests().when_done_countdown;
        let engine = fx.engine_with(countdown, when_done(action)).await;
        let mut events = engine.subscribe();
        let spec = throttled(4 * MIB, 4 * MIB);
        let paused = fx.add(&engine, "a.bin", &spec).await;
        engine.pause(paused).await.unwrap();
        let removed = fx.add(&engine, "b.bin", &spec).await;
        engine.remove(removed, true).await.unwrap();
        tokio::time::sleep(countdown * 3).await;
        let seen = when_done_events(&drain(&mut events));
        assert!(
            seen.iter().all(|e| matches!(e, Event::QueueIdle)),
            "{action:?}: {seen:?}"
        );
        assert_eq!(queue_idles(&seen), 2, "{action:?}: {seen:?}");

        // A job that finishes still runs it.
        engine.resume(paused).await.unwrap();
        events_until(&mut events, |e| match action {
            WhenDone::Sleep => is_countdown(e),
            _ => is_fired(e),
        })
        .await;
        engine.shutdown().await.unwrap();
    }
}

/// The settings the engine announced last among `events`.
fn last_settings(events: &[Event]) -> Option<Settings> {
    events.iter().rev().find_map(|e| match e {
        Event::SettingsChanged(s) => Some(s.clone()),
        _ => None,
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sleep_and_shutdown_fire_once() {
    for action in [WhenDone::Sleep, WhenDone::Shutdown] {
        let fx = Fixture::new().await;
        let countdown = Timing::fast_for_tests().when_done_countdown;
        let engine = fx.engine_with(countdown, when_done(action)).await;
        let mut events = engine.subscribe();
        fx.add(&engine, "a.bin", &FileSpec::new(MIB)).await;
        events_until(&mut events, is_fired).await;
        // Even when the engine stops right after firing.
        engine.shutdown().await.unwrap();
        let after = drain(&mut events);
        assert_eq!(
            last_settings(&after).map(|s| s.when_done),
            Some(WhenDone::Nothing),
            "{action:?}: {after:?}"
        );
        assert_eq!(engine.settings().when_done, WhenDone::Nothing);
        let reopened = fx.engine().await;
        assert_eq!(
            reopened.settings().when_done,
            WhenDone::Nothing,
            "{action:?}"
        );
        reopened.shutdown().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn notify_and_quit_stay_set() {
    for action in [WhenDone::Notify, WhenDone::Quit] {
        let fx = Fixture::new().await;
        let engine = fx
            .engine_with(Duration::from_secs(30), when_done(action))
            .await;
        let mut events = engine.subscribe();
        fx.add(&engine, "a.bin", &FileSpec::new(MIB)).await;
        events_until(&mut events, is_fired).await;
        engine.shutdown().await.unwrap();
        assert_eq!(last_settings(&drain(&mut events)), None);
        let reopened = fx.engine().await;
        assert_eq!(reopened.settings().when_done, action);
        reopened.shutdown().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn keep_awake_follows_downloads() {
    let fx = Fixture::new().await;
    let engine = fx.engine().await;
    assert!(!engine.keeping_awake());
    let id = fx.add(&engine, "a.bin", &throttled(4 * MIB, 4 * MIB)).await;
    assert_eq!(view(&engine, id).state, JobState::Downloading);
    assert!(engine.keeping_awake());
    assert_eq!(wait(&engine, id).await, JobState::Completed);
    assert!(!engine.keeping_awake());

    let no_keep_awake = Settings {
        keep_awake: false,
        ..Settings::default()
    };
    engine.set_settings(no_keep_awake.clone()).await.unwrap();
    let id = fx.add(&engine, "b.bin", &throttled(4 * MIB, 4 * MIB)).await;
    let sampler = {
        let engine = engine.clone();
        tokio::spawn(async move {
            let mut ever = false;
            while view(&engine, id).state == JobState::Downloading {
                ever |= engine.keeping_awake();
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            ever
        })
    };
    assert_eq!(wait(&engine, id).await, JobState::Completed);
    assert!(!sampler.await.unwrap(), "kept awake with keep_awake off");
    assert!(!engine.keeping_awake());

    // Turning it on while a job downloads keeps the computer awake at once.
    let id = fx.add(&engine, "c.bin", &throttled(4 * MIB, 4 * MIB)).await;
    assert!(!engine.keeping_awake());
    engine.set_settings(Settings::default()).await.unwrap();
    assert_eq!(view(&engine, id).state, JobState::Downloading);
    assert!(engine.keeping_awake());
    engine.shutdown().await.unwrap();
    assert!(!engine.keeping_awake());
}
