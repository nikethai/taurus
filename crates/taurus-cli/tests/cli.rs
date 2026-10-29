use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::time::Duration;

use taurus_core::WhenDone;
use taurus_core::system::command_for;
use taurus_testkit::{FileSpec, TestServer, content, sha256_hex};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

const MIB: usize = 1024 * 1024;
const PATH: &str = "/x.bin";
const TIMEOUT: Duration = Duration::from_secs(60);

struct Fixture {
    tmp: tempfile::TempDir,
    out: PathBuf,
    server: TestServer,
}

impl Fixture {
    async fn new() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("out");
        fs::create_dir(&out).unwrap();
        Fixture {
            tmp,
            out,
            server: TestServer::start().await,
        }
    }

    /// `taurus --data-dir <tmp>/data`, never reading the user's real data dir.
    fn taurus(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_taurus"));
        cmd.env_remove("TAURUS_DATA_DIR")
            .arg("--data-dir")
            .arg(self.tmp.path().join("data"))
            .stdin(Stdio::null())
            .kill_on_drop(true);
        cmd
    }

    async fn run(&self, args: &[&str]) -> Output {
        let mut cmd = self.taurus();
        cmd.args(args);
        output(cmd).await
    }

    async fn add(&self, url: &str, dir: &Path) -> Output {
        self.run(&["add", url, "--dir", dir.to_str().unwrap()])
            .await
    }

    fn file(&self) -> PathBuf {
        self.out.join("x.bin")
    }
}

async fn output(mut cmd: Command) -> Output {
    tokio::time::timeout(TIMEOUT, cmd.output())
        .await
        .expect("taurus did not finish in 60 s")
        .expect("run taurus")
}

fn assert_exit(output: &Output, code: i32) {
    assert_eq!(
        output.status.code(),
        Some(code),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn assert_same_bytes(path: &Path, spec: &FileSpec) {
    let got = fs::read(path).unwrap();
    assert_eq!(got.len(), spec.size);
    assert_eq!(sha256_hex(&got), sha256_hex(&content(spec)));
}

#[tokio::test(flavor = "multi_thread")]
async fn add_downloads_file() {
    let fx = Fixture::new().await;
    let spec = FileSpec::new(3 * MIB);
    let url = fx.server.serve(PATH, spec.clone());

    let output = fx.add(&url, &fx.out).await;

    assert_exit(&output, 0);
    assert_same_bytes(&fx.file(), &spec);
}

#[tokio::test(flavor = "multi_thread")]
async fn json_list_shows_completed() {
    let fx = Fixture::new().await;
    let url = fx.server.serve(PATH, FileSpec::new(3 * MIB));
    assert_exit(&fx.add(&url, &fx.out).await, 0);

    let output = fx.run(&["--json", "list"]).await;

    assert_exit(&output, 0);
    let jobs: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let jobs = jobs.as_array().expect("list --json prints an array");
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0]["state"], "Completed");
    assert_eq!(jobs[0]["file_name"], "x.bin");
}

#[tokio::test(flavor = "multi_thread")]
async fn kill_minus_9_then_resume_is_byte_perfect() {
    tokio::time::timeout(TIMEOUT, kill_then_resume())
        .await
        .expect("kill and resume did not finish in 60 s");
}

async fn kill_then_resume() {
    let fx = Fixture::new().await;
    let mut spec = FileSpec::new(32 * MIB);
    spec.bytes_per_sec = Some(8 * MIB as u64);
    let url = fx.server.serve(PATH, spec.clone());

    let mut child = fx
        .taurus()
        .args(["--json", "add", &url, "--dir", fx.out.to_str().unwrap()])
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn taurus");
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    loop {
        let line = lines
            .next_line()
            .await
            .unwrap()
            .expect("taurus ended before downloading 12 MiB");
        let event: serde_json::Value = serde_json::from_str(&line).unwrap();
        let downloaded = event["Progress"]["downloaded"].as_u64().unwrap_or(0);
        // 1.5 s in at 8 MiB/s: past the ~1 s progress commit cadence.
        if downloaded > 12 * MIB as u64 {
            break;
        }
    }
    // SIGKILL on Unix, TerminateProcess on Windows: no chance to save anything.
    // `kill` also waits for the process to exit.
    child.kill().await.unwrap();

    assert!(!fx.file().exists(), "the download finished before the kill");
    assert!(fx.out.join("x.bin.tapa").exists());
    // Lets the server finish counting writes that were in flight.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let served_at_kill = fx.server.bytes_served(PATH);
    let requests_before = fx.server.requests(PATH);

    let list = fx.run(&["--json", "list"]).await;
    assert_exit(&list, 0);
    let jobs: serde_json::Value = serde_json::from_slice(&list.stdout).unwrap();
    assert_eq!(jobs[0]["state"], "Queued");
    let durable = jobs[0]["downloaded"].as_u64().unwrap();
    assert!(durable > 0, "no progress was saved before the kill");

    let output = fx.run(&["resume", "1"]).await;

    assert_exit(&output, 0);
    assert_same_bytes(&fx.file(), &spec);
    assert!(fx.server.requests(PATH) > requests_before);
    let served = fx.server.bytes_served(PATH);
    // A restart from zero would serve the whole file again.
    let resumed = served - served_at_kill;
    assert!(
        resumed < spec.size as u64,
        "resume served {resumed} bytes for a {} byte file ({durable} saved)",
        spec.size
    );
    assert!(
        served < 2 * spec.size as u64,
        "served {served} bytes for a {} byte file",
        spec.size
    );
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn ctrl_c_stops_and_run_continues() {
    tokio::time::timeout(TIMEOUT, ctrl_c_then_run())
        .await
        .expect("Ctrl-C and run did not finish in 60 s");
}

#[cfg(unix)]
async fn ctrl_c_then_run() {
    let fx = Fixture::new().await;
    let mut spec = FileSpec::new(32 * MIB);
    spec.bytes_per_sec = Some(8 * MIB as u64);
    let url = fx.server.serve(PATH, spec.clone());

    let mut child = fx
        .taurus()
        .args(["add", &url, "--dir", fx.out.to_str().unwrap()])
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn taurus");
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let first = lines
        .next_line()
        .await
        .unwrap()
        .expect("taurus ended before showing progress");
    assert!(first.starts_with("#1 x.bin"), "{first}");
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let pid = child.id().unwrap().to_string();
    let sent = std::process::Command::new("kill")
        .args(["-INT", &pid])
        .status()
        .unwrap();
    assert!(sent.success());
    let mut rest = Vec::new();
    while let Some(line) = lines.next_line().await.unwrap() {
        rest.push(line);
    }
    let status = child.wait().await.unwrap();

    assert_eq!(status.code(), Some(2), "{rest:?}");
    assert!(
        rest.iter()
            .any(|l| l == "Stopped. Unfinished downloads continue with: taurus run"),
        "{rest:?}"
    );
    assert!(!rest.iter().any(|l| l.contains("Paused")), "{rest:?}");
    let list = fx.run(&["--json", "list"]).await;
    let jobs: serde_json::Value = serde_json::from_slice(&list.stdout).unwrap();
    assert_eq!(jobs[0]["state"], "Queued");
    assert!(jobs[0]["downloaded"].as_u64().unwrap() > 0);

    let output = fx.run(&["run"]).await;

    assert_exit(&output, 0);
    assert_same_bytes(&fx.file(), &spec);
    let served = fx.server.bytes_served(PATH);
    assert!(
        served < 2 * spec.size as u64,
        "served {served} bytes for a {} byte file",
        spec.size
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn missing_dir_exits_1() {
    let fx = Fixture::new().await;
    let url = fx.server.serve(PATH, FileSpec::new(MIB));

    let output = fx.add(&url, &fx.tmp.path().join("nope")).await;

    assert_exit(&output, 1);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("FolderUnavailable"), "stderr: {stderr}");
}

#[tokio::test(flavor = "multi_thread")]
async fn second_process_beside_running_add() {
    tokio::time::timeout(TIMEOUT, beside_running_add())
        .await
        .expect("did not finish in 60 s");
}

async fn beside_running_add() {
    let fx = Fixture::new().await;
    let mut spec = FileSpec::new(16 * MIB);
    spec.bytes_per_sec = Some(4 * MIB as u64);
    let url = fx.server.serve(PATH, spec.clone());

    let mut child = fx
        .taurus()
        .args(["--json", "add", &url, "--dir", fx.out.to_str().unwrap()])
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn taurus");
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    loop {
        let line = lines
            .next_line()
            .await
            .unwrap()
            .expect("taurus ended before any progress");
        if line.contains("\"Progress\"") {
            break;
        }
    }

    // `list` reads the stored state without touching the running job.
    let list = fx.run(&["--json", "list"]).await;
    assert_exit(&list, 0);
    let jobs: serde_json::Value = serde_json::from_slice(&list.stdout).unwrap();
    assert_eq!(jobs.as_array().unwrap().len(), 1);
    assert_eq!(jobs[0]["state"], "Downloading");

    // A second download of the same job is refused.
    let resume = fx.run(&["resume", "1"]).await;
    assert_exit(&resume, 1);
    let stderr = String::from_utf8_lossy(&resume.stderr);
    assert!(
        stderr.contains("another taurus instance"),
        "stderr: {stderr}"
    );

    while lines.next_line().await.unwrap().is_some() {}
    let status = child.wait().await.unwrap();
    assert_eq!(status.code(), Some(0));
    assert_same_bytes(&fx.file(), &spec);
    assert!(
        fx.server.bytes_served(PATH) < 2 * spec.size as u64,
        "served {} bytes",
        fx.server.bytes_served(PATH)
    );
    let list = fx.run(&["--json", "list"]).await;
    let jobs: serde_json::Value = serde_json::from_slice(&list.stdout).unwrap();
    assert_eq!(jobs[0]["state"], "Completed");
}

#[tokio::test(flavor = "multi_thread")]
async fn add_several_urls_and_file() {
    let fx = Fixture::new().await;
    let names = ["a.bin", "b.bin", "c.bin", "d.bin"];
    let specs: Vec<FileSpec> = (0..names.len())
        .map(|i| {
            let mut spec = FileSpec::new(MIB + i * 1000);
            spec.seed = i as u64 + 1;
            spec
        })
        .collect();
    let urls: Vec<String> = names
        .iter()
        .zip(&specs)
        .map(|(name, spec)| fx.server.serve(&format!("/{name}"), spec.clone()))
        .collect();
    let list = fx.tmp.path().join("urls.txt");
    fs::write(
        &list,
        format!("# two more\n{}\n\n  \n{}\n", urls[2], urls[3]),
    )
    .unwrap();

    let output = fx
        .run(&[
            "add",
            &urls[0],
            &urls[1],
            "--file",
            list.to_str().unwrap(),
            "--dir",
            fx.out.to_str().unwrap(),
        ])
        .await;

    assert_exit(&output, 0);
    for (name, spec) in names.iter().zip(&specs) {
        assert_same_bytes(&fx.out.join(name), spec);
    }
    let list = fx.run(&["--json", "list"]).await;
    let jobs: serde_json::Value = serde_json::from_slice(&list.stdout).unwrap();
    assert_eq!(jobs.as_array().unwrap().len(), 4);
}

#[tokio::test(flavor = "multi_thread")]
async fn config_roundtrip() {
    let fx = Fixture::new().await;

    assert_exit(&fx.run(&["config", "set", "max_active", "2"]).await, 0);
    assert_exit(&fx.run(&["config", "set", "speed", "5MB/s"]).await, 0);
    let output = fx.run(&["--json", "config", "get"]).await;

    assert_exit(&output, 0);
    let settings: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(settings["max_active"], 2);
    assert_eq!(
        settings["speed"],
        serde_json::json!({"Custom": {"bytes_per_sec": 5_000_000}})
    );
    assert_eq!(settings["when_done"], "Nothing");

    let output = fx.run(&["config", "get"]).await;
    assert_exit(&output, 0);
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("max_active = 2\n"), "stdout: {text}");
    assert!(text.contains("speed = 5MB/s\n"), "stdout: {text}");
    let output = fx.run(&["config", "get", "keep_awake"]).await;
    assert_exit(&output, 0);
    assert_eq!(String::from_utf8_lossy(&output.stdout), "keep_awake = on\n");
}

#[tokio::test(flavor = "multi_thread")]
async fn invalid_config_value_exits_1() {
    let fx = Fixture::new().await;

    let output = fx.run(&["config", "set", "speed", "fast"]).await;

    assert_exit(&output, 1);
    let stderr = String::from_utf8_lossy(&output.stderr);
    for allowed in ["fastest", "balanced", "gentle", "<N>KB/s", "<N>MB/s"] {
        assert!(stderr.contains(allowed), "stderr: {stderr}");
    }
    // Nothing was saved.
    let output = fx.run(&["config", "get", "speed"]).await;
    assert_eq!(String::from_utf8_lossy(&output.stdout), "speed = fastest\n");
    // The engine refuses a queue that could never run.
    assert_exit(&fx.run(&["config", "set", "max_active", "0"]).await, 1);
    assert_exit(&fx.run(&["config", "get", "nope"]).await, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn mirror_with_two_urls_is_rejected() {
    let fx = Fixture::new().await;
    let url = fx.server.serve(PATH, FileSpec::new(MIB));

    let output = fx
        .run(&[
            "add",
            &url,
            &url,
            "--mirror",
            &url,
            "--dir",
            fx.out.to_str().unwrap(),
        ])
        .await;

    assert_ne!(output.status.code(), Some(0));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("exactly one URL"), "stderr: {stderr}");
    assert_eq!(fx.server.requests(PATH), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn when_done_dry_run_prints_command() {
    let fx = Fixture::new().await;
    let spec = FileSpec::new(MIB);
    let url = fx.server.serve(PATH, spec.clone());
    assert_exit(&fx.run(&["config", "set", "when_done", "sleep"]).await, 0);

    let mut cmd = fx.taurus();
    // Never a real sleep: only `--dry-run-power` keeps the machine awake.
    cmd.env("TAURUS_FAST_TIMING", "1").args([
        "--dry-run-power",
        "add",
        &url,
        "--dir",
        fx.out.to_str().unwrap(),
    ]);
    let output = output(cmd).await;

    assert_exit(&output, 0);
    assert_same_bytes(&fx.file(), &spec);
    let stdout = String::from_utf8_lossy(&output.stdout);
    if let Some((program, args)) = command_for(WhenDone::Sleep) {
        let line = std::iter::once(program)
            .chain(args)
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            stdout.contains(&format!("would run: {line}")),
            "stdout: {stdout}"
        );
    }
    // Sleep is one-shot.
    let get = fx.run(&["config", "get", "when_done"]).await;
    assert_exit(&get, 0);
    assert_eq!(
        String::from_utf8_lossy(&get.stdout),
        "when_done = nothing\n"
    );
}
