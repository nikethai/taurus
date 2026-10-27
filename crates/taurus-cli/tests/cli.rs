use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::time::Duration;

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
        tokio::time::timeout(TIMEOUT, cmd.output())
            .await
            .expect("taurus did not finish in 60 s")
            .expect("run taurus")
    }

    async fn add(&self, url: &str, dir: &Path) -> Output {
        self.run(&["add", url, "--dir", dir.to_str().unwrap()])
            .await
    }

    fn file(&self) -> PathBuf {
        self.out.join("x.bin")
    }
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
    assert_eq!(jobs[0]["state"], "Paused");
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

#[tokio::test(flavor = "multi_thread")]
async fn missing_dir_exits_1() {
    let fx = Fixture::new().await;
    let url = fx.server.serve(PATH, FileSpec::new(MIB));

    let output = fx.add(&url, &fx.tmp.path().join("nope")).await;

    assert_exit(&output, 1);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("FolderUnavailable"), "stderr: {stderr}");
}
