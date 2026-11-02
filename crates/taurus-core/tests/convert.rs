//! Converting finished files with the fake ffmpeg: names, never
//! overwriting, failures and stops, the queue, and playlists.
//!
//! The fake ffmpeg reads `FAKE_FFMPEG_FAIL` and `FAKE_FFMPEG_STEP_MS` from
//! the environment, which the test process shares: every test holds `serial`
//! for its whole run, so one test's setting never reaches another's ffmpeg.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};
use taurus_core::media::ConvertPreset;
use taurus_core::tools::ToolSources;
use taurus_core::{
    AddRequest, ConvertSource, Engine, EngineConfig, ErrorKind, Event, JobId, JobKind, JobState,
    JobView, Timing,
};
use taurus_testkit::{
    FakeToolHost, ToolRelease, process_args_log, set_ytdlp_scenario, unique_version,
};
use tokio::sync::MutexGuard;

static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn serial() -> MutexGuard<'static, ()> {
    SERIAL.lock().await
}

/// Sets an environment variable of the fake ffmpeg until dropped.
struct Env(&'static str);

impl Env {
    fn set(name: &'static str, value: &str) -> Env {
        // SAFETY: every test of this binary holds `serial`, and nothing here
        // reads the environment concurrently.
        unsafe { std::env::set_var(name, value) };
        Env(name)
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        // SAFETY: as in `set`.
        unsafe { std::env::remove_var(self.0) };
    }
}

struct Rig {
    host: FakeToolHost,
    tmp: tempfile::TempDir,
    engine: Engine,
    /// The folder of the files to convert.
    dir: PathBuf,
}

impl Rig {
    async fn new() -> Rig {
        Rig::with(&json!([]), |_| {}).await
    }

    /// An engine (fast timing) whose yt-dlp replays `scenario`.
    async fn with(scenario: &Value, tweak: impl FnOnce(&mut EngineConfig)) -> Rig {
        let version = unique_version();
        set_ytdlp_scenario(&version, &scenario.to_string());
        let host = FakeToolHost::start(
            ToolRelease::new(&version),
            ToolRelease::new("7.1"),
            ToolRelease::new("2.5.0"),
        )
        .await;
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("dl");
        std::fs::create_dir_all(&dir).unwrap();
        // As the engine records it: no `\\?\` prefix on Windows.
        let dir = dunce::canonicalize(&dir).unwrap();
        let mut config = EngineConfig::new(tmp.path().join("data"));
        config.tool_sources = ToolSources::under(&url::Url::parse(&host.base_url()).unwrap());
        config.timing = Timing::fast_for_tests();
        tweak(&mut config);
        let engine = Engine::open(config).await.unwrap();
        Rig {
            host,
            tmp,
            engine,
            dir,
        }
    }

    /// A file of `size` bytes in the folder.
    fn file(&self, name: &str, size: usize) -> PathBuf {
        let path = self.dir.join(name);
        let bytes: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
        std::fs::write(&path, bytes).unwrap();
        path
    }

    fn view(&self, id: JobId) -> JobView {
        self.engine
            .snapshot()
            .into_iter()
            .find(|v| v.id == id)
            .unwrap()
    }

    async fn wait(&self, id: JobId) -> JobState {
        tokio::time::timeout(Duration::from_secs(60), self.engine.wait(id))
            .await
            .expect("the job did not finish")
    }

    /// Waits up to 30 s for the job's view to satisfy `ok`.
    async fn until(&self, id: JobId, what: &str, ok: impl Fn(&JobView) -> bool) -> JobView {
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        loop {
            let view = self.view(id);
            if ok(&view) {
                return view;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for {what}: {view:?}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// The names in the folder, sorted.
    fn names(&self) -> Vec<String> {
        names_in(&self.dir)
    }
}

fn names_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// ffmpeg's calls that were given `source`.
fn ffmpeg_calls(source: &Path) -> Vec<Vec<String>> {
    let source = source.to_string_lossy();
    process_args_log()
        .into_iter()
        .filter(|e| e.tool == "ffmpeg" && e.argv.iter().any(|a| *a == source))
        .map(|e| e.argv)
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn convert_job_completes() {
    let _serial = serial().await;
    let rig = Rig::new().await;
    let source = rig.file("song.webm", 4000);
    let before = std::fs::read(&source).unwrap();
    let mut events = rig.engine.subscribe();

    let id = rig
        .engine
        .convert(ConvertSource::Path(source.clone()), ConvertPreset::Mp3)
        .await
        .unwrap();
    assert_eq!(rig.view(id).kind, JobKind::Convert);
    assert_eq!(rig.view(id).file_name, "song (mp3).mp3");
    assert_eq!(rig.wait(id).await, JobState::Completed);

    let output = rig.dir.join("song (mp3).mp3");
    // The fake copies its input: the output is what ffmpeg wrote.
    assert_eq!(std::fs::read(&output).unwrap(), before);
    assert_eq!(
        std::fs::read(&source).unwrap(),
        before,
        "the source is intact"
    );
    // Nothing else is left: no temporary file.
    assert_eq!(rig.names(), ["song (mp3).mp3", "song.webm"]);
    let view = rig.view(id);
    assert_eq!(view.file_name, "song (mp3).mp3");
    assert_eq!(view.total, Some(before.len() as u64));
    assert_eq!(view.error, None);

    // Progress ran on the media time (ffprobe said 3 s, the fake printed
    // 1 s, 2 s and 3 s), then showed the size.
    let mut seen = Vec::new();
    while let Ok(event) = events.try_recv() {
        if let Event::Progress {
            id: event_id,
            downloaded,
            total,
            ..
        } = event
            && event_id == id
        {
            seen.push((downloaded, total));
        }
    }
    for step in [1000, 2000, 3000] {
        assert!(seen.contains(&(step, Some(3000))), "{step} in {seen:?}");
    }
    assert_eq!(seen.last(), Some(&(4000, Some(4000))));

    // ffmpeg never overwrites, and never writes the final name itself.
    let calls = ffmpeg_calls(&source);
    assert_eq!(calls.len(), 1, "{calls:?}");
    let argv = &calls[0];
    assert!(argv.iter().any(|a| a == "-n") && !argv.iter().any(|a| a == "-y"));
    assert_ne!(
        argv.last().map(Path::new),
        Some(output.as_path()),
        "{argv:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn smaller_mp4_names_and_args() {
    let _serial = serial().await;
    let rig = Rig::new().await;
    let source = rig.file("clip.mkv", 600);
    let id = rig
        .engine
        .convert(
            ConvertSource::Path(source.clone()),
            ConvertPreset::SmallerMp4,
        )
        .await
        .unwrap();
    assert_eq!(rig.wait(id).await, JobState::Completed);
    assert_eq!(rig.names(), ["clip (smaller).mp4", "clip.mkv"]);
    let argv = &ffmpeg_calls(&source)[0];
    let joined = argv.join(" ");
    assert!(
        joined.contains("-c:v libx264 -crf 28 -preset veryfast"),
        "{joined}"
    );
    assert!(
        joined.contains("-movflags +faststart -progress pipe:1"),
        "{joined}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn convert_never_overwrites() {
    let _serial = serial().await;
    let rig = Rig::new().await;
    let source = rig.file("song.webm", 400);
    let taken = rig.dir.join("song (mp3).mp3");
    std::fs::write(&taken, b"somebody else's file").unwrap();

    let id = rig
        .engine
        .convert(ConvertSource::Path(source), ConvertPreset::Mp3)
        .await
        .unwrap();
    assert_eq!(rig.view(id).file_name, "song (mp3) (1).mp3");
    assert_eq!(rig.wait(id).await, JobState::Completed);

    assert_eq!(std::fs::read(&taken).unwrap(), b"somebody else's file");
    assert_eq!(
        std::fs::read(rig.dir.join("song (mp3) (1).mp3"))
            .unwrap()
            .len(),
        400
    );
    assert_eq!(
        rig.names(),
        ["song (mp3) (1).mp3", "song (mp3).mp3", "song.webm"]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_name_taken_after_the_job_was_added_is_never_replaced() {
    let _serial = serial().await;
    // Not started: the name is chosen at add, and taken before the run.
    let rig = Rig::with(&json!([]), |c| c.autostart = false).await;
    let source = rig.file("song.webm", 400);
    let id = rig
        .engine
        .convert(ConvertSource::Path(source), ConvertPreset::Mp3)
        .await
        .unwrap();
    assert_eq!(rig.view(id).file_name, "song (mp3).mp3");
    let taken = rig.dir.join("song (mp3).mp3");
    std::fs::write(&taken, b"created meanwhile").unwrap();

    rig.engine.start_now(id).await.unwrap();
    assert_eq!(rig.wait(id).await, JobState::Completed);
    assert_eq!(std::fs::read(&taken).unwrap(), b"created meanwhile");
    assert_eq!(rig.view(id).file_name, "song (mp3) (1).mp3");
    assert_eq!(
        rig.names(),
        ["song (mp3) (1).mp3", "song (mp3).mp3", "song.webm"]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failed_convert_keeps_source() {
    let _serial = serial().await;
    let _fail = Env::set("FAKE_FFMPEG_FAIL", "1");
    let rig = Rig::new().await;
    let source = rig.file("song.webm", 4000);
    let before = std::fs::read(&source).unwrap();
    // A file that is not the job's, with the name the job would use.
    let other = rig.dir.join("song (mp3).mp3");
    std::fs::write(&other, b"keep me").unwrap();

    let id = rig
        .engine
        .convert(ConvertSource::Path(source.clone()), ConvertPreset::Mp3)
        .await
        .unwrap();
    assert_eq!(rig.wait(id).await, JobState::Failed);

    let view = rig.view(id);
    let (kind, detail) = view.error.expect("an error");
    assert_eq!(kind, ErrorKind::ToolFailed, "{detail}");
    assert!(detail.contains("conversion failed halfway"), "{detail}");
    // Only the job's own partial output is gone.
    assert_eq!(rig.names(), ["song (mp3).mp3", "song.webm"]);
    assert_eq!(std::fs::read(&source).unwrap(), before);
    assert_eq!(std::fs::read(&other).unwrap(), b"keep me");
    // ffmpeg did write something before failing.
    assert_eq!(ffmpeg_calls(&source).len(), 1);
}

#[cfg(unix)]
fn process_gone(pid: u32) -> bool {
    let pid = pid as libc::pid_t;
    let gone = |target: libc::pid_t| {
        // SAFETY: signal 0 only checks that the target exists.
        let found = unsafe { libc::kill(target, 0) } == 0;
        !found && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
    };
    gone(pid) && gone(-pid)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pausing_stops_ffmpeg_and_removes_its_partial_output() {
    let _serial = serial().await;
    // 3 steps of 400 ms: the job runs long enough to be paused.
    let _slow = Env::set("FAKE_FFMPEG_STEP_MS", "400");
    let rig = Rig::new().await;
    let source = rig.file("song.webm", 4000);
    let id = rig
        .engine
        .convert(ConvertSource::Path(source.clone()), ConvertPreset::Mp3)
        .await
        .unwrap();
    rig.until(id, "ffmpeg under way", |v| {
        v.state == JobState::Downloading && v.downloaded > 0
    })
    .await;
    // The partial output exists while ffmpeg runs.
    assert_eq!(rig.names().len(), 2, "{:?}", rig.names());

    rig.engine.pause(id).await.unwrap();
    assert_eq!(rig.view(id).state, JobState::Paused);
    assert_eq!(rig.names(), ["song.webm"]);
    assert_eq!(std::fs::read(&source).unwrap().len(), 4000);
    #[cfg(unix)]
    {
        let pids: Vec<u32> = process_args_log()
            .into_iter()
            .filter(|e| e.tool == "ffmpeg" && e.argv.iter().any(|a| *a == source.to_string_lossy()))
            .map(|e| e.pid)
            .collect();
        assert_eq!(pids.len(), 1);
        assert!(process_gone(pids[0]), "ffmpeg is still running");
    }

    // Resumed, it converts again, from the start.
    drop(_slow);
    rig.engine.resume(id).await.unwrap();
    assert_eq!(rig.wait(id).await, JobState::Completed);
    assert_eq!(rig.names(), ["song (mp3).mp3", "song.webm"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn removing_a_running_convert_stops_it_and_keeps_every_other_file() {
    let _serial = serial().await;
    let _slow = Env::set("FAKE_FFMPEG_STEP_MS", "400");
    let rig = Rig::new().await;
    let source = rig.file("song.webm", 4000);
    let id = rig
        .engine
        .convert(ConvertSource::Path(source), ConvertPreset::Mp3)
        .await
        .unwrap();
    rig.until(id, "ffmpeg under way", |v| v.downloaded > 0)
        .await;
    rig.engine.remove(id, true).await.unwrap();
    assert_eq!(rig.names(), ["song.webm"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn removing_a_finished_convert_deletes_only_its_output() {
    let _serial = serial().await;
    let rig = Rig::new().await;
    let source = rig.file("song.webm", 400);
    let id = rig
        .engine
        .convert(ConvertSource::Path(source), ConvertPreset::Mp3)
        .await
        .unwrap();
    assert_eq!(rig.wait(id).await, JobState::Completed);

    rig.engine.remove(id, false).await.unwrap();
    assert_eq!(rig.names(), ["song (mp3).mp3", "song.webm"]);

    let id = rig
        .engine
        .convert(
            ConvertSource::Path(rig.dir.join("song.webm")),
            ConvertPreset::Mp3,
        )
        .await
        .unwrap();
    assert_eq!(rig.view(id).file_name, "song (mp3) (1).mp3");
    assert_eq!(rig.wait(id).await, JobState::Completed);
    rig.engine.remove(id, true).await.unwrap();
    assert_eq!(rig.names(), ["song (mp3).mp3", "song.webm"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn convert_jobs_count_toward_max_active() {
    let _serial = serial().await;
    let _slow = Env::set("FAKE_FFMPEG_STEP_MS", "300");
    let rig = Rig::new().await;
    let mut settings = rig.engine.settings();
    settings.max_active = 1;
    rig.engine.set_settings(settings).await.unwrap();
    let first = rig.file("one.webm", 400);
    let second = rig.file("two.webm", 400);
    let a = rig
        .engine
        .convert(ConvertSource::Path(first), ConvertPreset::Mp3)
        .await
        .unwrap();
    let b = rig
        .engine
        .convert(ConvertSource::Path(second), ConvertPreset::Mp3)
        .await
        .unwrap();
    rig.until(a, "the first running", |v| v.state == JobState::Downloading)
        .await;
    assert_eq!(rig.view(b).state, JobState::Queued);
    assert_eq!(rig.wait(a).await, JobState::Completed);
    assert_eq!(rig.wait(b).await, JobState::Completed);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_convert_waits_for_ffmpeg_to_be_installed() {
    let _serial = serial().await;
    let rig = Rig::new().await;
    rig.host.go_offline();
    let source = rig.file("song.webm", 400);
    let id = rig
        .engine
        .convert(ConvertSource::Path(source), ConvertPreset::Mp3)
        .await
        .unwrap();
    let view = rig
        .until(id, "waiting for the tool", |v| v.state == JobState::Waiting)
        .await;
    assert_eq!(view.error.map(|e| e.0), Some(ErrorKind::ConnectionLost));
    assert_eq!(rig.names(), ["song.webm"]);

    rig.host.go_online();
    assert_eq!(rig.wait(id).await, JobState::Completed);
    assert_eq!(rig.names(), ["song (mp3).mp3", "song.webm"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sources_that_cannot_be_converted_are_refused() {
    let _serial = serial().await;
    let rig = Rig::new().await;
    let mp3 = |source| rig.engine.convert(source, ConvertPreset::Mp3);
    // Missing, or not a file.
    let gone = rig.dir.join("gone.webm");
    assert!(mp3(ConvertSource::Path(gone)).await.is_err());
    assert!(mp3(ConvertSource::Path(rig.dir.clone())).await.is_err());
    assert!(mp3(ConvertSource::Job(JobId(999))).await.is_err());
    // Still downloading, by its extension.
    let part = rig.file("video.mp4.part", 10);
    let err = mp3(ConvertSource::Path(part)).await.unwrap_err();
    assert!(err.detail.contains("still downloading"), "{err}");
    assert_eq!(rig.names(), ["video.mp4.part"]);
    assert!(rig.engine.snapshot().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_job_still_downloading_is_refused() {
    let _serial = serial().await;
    // Added but not started: a Queued media job.
    let (info, _) = playlist("PLq", "Queued", &["a"]);
    let scenario = json!([{ "if_arg": "-J", "stdout": [info.to_string()] }]);
    let rig = Rig::with(&scenario, |c| c.autostart = false).await;
    let id = rig
        .engine
        .add(AddRequest {
            url: "https://www.youtube.com/playlist?list=PLstill_queued".into(),
            dest_dir: rig.dir.clone(),
            ..AddRequest::default()
        })
        .await
        .unwrap();
    let err = rig
        .engine
        .convert(ConvertSource::Job(id), ConvertPreset::Mp3)
        .await
        .unwrap_err();
    assert!(err.detail.contains("not finished"), "{err}");
    assert_eq!(rig.engine.snapshot().len(), 1);
}

/// A playlist's `-J` answer and the file names its entries are downloaded to.
fn playlist(id: &str, title: &str, entries: &[&str]) -> (Value, Vec<String>) {
    let items: Vec<Value> = entries
        .iter()
        .map(|e| json!({"_type": "url", "id": e, "title": e}))
        .collect();
    let info = json!({
        "_type": "playlist",
        "id": id,
        "title": title,
        "webpage_url": format!("https://www.youtube.com/playlist?list={id}"),
        "extractor_key": "YoutubeTab",
        "entries": items,
    });
    let files = entries
        .iter()
        .enumerate()
        .map(|(i, e)| format!("{title}/{:03} - {e} [{e}].mp4", i + 1))
        .collect();
    (info, files)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn convert_playlist_job_makes_one_job_per_file() {
    let _serial = serial().await;
    let (info, files) = playlist("PLc", "Mix", &["a", "b"]);
    let mut stdout = Vec::new();
    for (i, file) in files.iter().enumerate() {
        stdout.push(format!(
            "TAURUS-ENTRY {} 2 {} {}",
            i + 1,
            json!(format!("{i}")),
            json!("t")
        ));
        stdout.push(format!("TAURUS-FILE {}", json!(file)));
    }
    let write: Vec<Value> = files.iter().map(|f| json!([f, 300])).collect();
    let scenario = json!([
        { "if_arg": "-J", "stdout": [info.to_string()] },
        { "stdout": stdout, "write_files": write },
    ]);
    let rig = Rig::with(&scenario, |_| {}).await;
    let playlist_job = rig
        .engine
        .add(AddRequest {
            url: "https://www.youtube.com/playlist?list=PLconvert_playlist".into(),
            dest_dir: rig.dir.clone(),
            ..AddRequest::default()
        })
        .await
        .unwrap();
    assert_eq!(rig.wait(playlist_job).await, JobState::Completed);
    let before: Vec<Vec<u8>> = files
        .iter()
        .map(|f| std::fs::read(rig.dir.join(f)).unwrap())
        .collect();

    let ids = rig
        .engine
        .convert_many(ConvertSource::Job(playlist_job), ConvertPreset::Mp3)
        .await
        .unwrap();
    assert_eq!(ids.len(), 2);
    let converts: Vec<JobView> = rig
        .engine
        .snapshot()
        .into_iter()
        .filter(|v| v.kind == JobKind::Convert)
        .collect();
    assert_eq!(converts.len(), 2);
    // Each is named after its own output, inside the playlist's folder.
    assert_eq!(converts[0].file_name, "001 - a [a] (mp3).mp3");
    assert_eq!(converts[1].file_name, "002 - b [b] (mp3).mp3");
    for id in &ids {
        assert_eq!(rig.wait(*id).await, JobState::Completed);
    }
    let folder = rig.dir.join("Mix");
    assert_eq!(
        names_in(&folder),
        [
            "001 - a [a] (mp3).mp3",
            "001 - a [a].mp4",
            "002 - b [b] (mp3).mp3",
            "002 - b [b].mp4"
        ]
    );
    for (file, bytes) in files.iter().zip(&before) {
        assert_eq!(&std::fs::read(rig.dir.join(file)).unwrap(), bytes);
    }
    assert!(rig.tmp.path().join("data").is_dir());
}
