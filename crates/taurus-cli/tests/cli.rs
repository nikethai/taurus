use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::time::Duration;

use serde_json::{Value, json};
use taurus_core::WhenDone;
use taurus_core::system::command_for;
use taurus_testkit::{
    FakeToolHost, FileSpec, TestServer, ToolRelease, TorrentSeeder, content, process_args_log,
    set_ytdlp_scenario, sha256_hex, unique_version,
};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

const MIB: usize = 1024 * 1024;
const PATH: &str = "/x.bin";
const TIMEOUT: Duration = Duration::from_secs(60);

struct Fixture {
    tmp: tempfile::TempDir,
    out: PathBuf,
    server: TestServer,
    /// The fake tool host's base URL, for the video tests.
    tool_base: Option<String>,
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
            tool_base: None,
        }
    }

    /// `taurus --data-dir <tmp>/data`, never reading the user's real data dir.
    fn taurus(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_taurus"));
        // Passwords stay in memory (never the real keychain) and none comes
        // from the environment unless a test sets one.
        cmd.env_remove("TAURUS_DATA_DIR")
            .env_remove("TAURUS_PASSWORD")
            .env("TAURUS_SECRETS_IN_MEMORY", "1")
            .arg("--data-dir")
            .arg(self.tmp.path().join("data"))
            .stdin(Stdio::null())
            .kill_on_drop(true);
        if let Some(base) = &self.tool_base {
            // The fake yt-dlp's scenario and args log reach the child through
            // the environment the testkit set for this process.
            cmd.env("TAURUS_TOOL_SOURCES", base)
                .env("TAURUS_FAST_TIMING", "1");
        }
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

/// Two small files in a folder torrent named `pack`, seeded locally.
async fn pack_seeder() -> (TorrentSeeder, Vec<u8>, Vec<u8>) {
    let spec = |size, seed| {
        content(&FileSpec {
            seed,
            ..FileSpec::new(size)
        })
    };
    let (a, b) = (spec(300 * 1024, 1), spec(100 * 1024, 2));
    let seeder = TorrentSeeder::start(
        "pack",
        &[("a.bin", a.clone()), ("dir/b.bin", b.clone())],
        None,
    )
    .await;
    (seeder, a, b)
}

impl Fixture {
    /// Like `taurus`, with the test-only switch that gives every torrent the
    /// seeder as its one peer.
    fn taurus_with_peer(&self, seeder: &TorrentSeeder) -> Command {
        let mut cmd = self.taurus();
        cmd.env("TAURUS_TORRENT_TEST_PEERS", seeder.addr.to_string())
            .env("TAURUS_FAST_TIMING", "1");
        cmd
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn add_magnet_downloads() {
    let fx = Fixture::new().await;
    let (seeder, a, b) = pack_seeder().await;
    assert_exit(&fx.run(&["config", "set", "torrent.ratio", "0"]).await, 0);

    let mut cmd = fx.taurus_with_peer(&seeder);
    cmd.args(["add", &seeder.magnet, "--dir", fx.out.to_str().unwrap()]);
    let output = output(cmd).await;

    assert_exit(&output, 0);
    assert_eq!(fs::read(fx.out.join("pack/a.bin")).unwrap(), a);
    assert_eq!(fs::read(fx.out.join("pack/dir/b.bin")).unwrap(), b);
    let list = fx.run(&["list"]).await;
    let text = String::from_utf8_lossy(&list.stdout);
    assert!(
        text.contains("torrent") && text.contains("Completed"),
        "{text}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn add_torrent_file_with_files_and_seeding_note() {
    let fx = Fixture::new().await;
    let (seeder, a, _) = pack_seeder().await;
    let torrent = fx.tmp.path().join("pack.torrent");
    fs::write(&torrent, &seeder.torrent_bytes).unwrap();

    let mut cmd = fx.taurus_with_peer(&seeder);
    cmd.args(["add", torrent.to_str().unwrap(), "--files", "0"])
        .args(["--dir", fx.out.to_str().unwrap()]);
    let output = output(cmd).await;

    assert_exit(&output, 0);
    assert_eq!(fs::read(fx.out.join("pack/a.bin")).unwrap(), a);
    // Not downloaded: at most the few bytes of a piece shared with a.bin.
    let b_len = fs::metadata(fx.out.join("pack/dir/b.bin")).map_or(0, |m| m.len());
    assert!(
        b_len < 100 * 1024,
        "dir/b.bin was downloaded ({b_len} bytes)"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(
            "Sharing pauses when Taurus exits; it continues the next time Taurus runs \
             (up to ratio 1.0)."
        ),
        "stdout: {stdout}"
    );
    let list = fx.run(&["list"]).await;
    let text = String::from_utf8_lossy(&list.stdout);
    assert!(
        text.contains("Queued") && text.contains("torrent"),
        "{text}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn torrent_files_lists_json() {
    let fx = Fixture::new().await;
    let (seeder, _, _) = pack_seeder().await;
    let torrent = fx.tmp.path().join("pack.torrent");
    fs::write(&torrent, &seeder.torrent_bytes).unwrap();

    let output = fx
        .run(&["--json", "torrent-files", torrent.to_str().unwrap()])
        .await;

    assert_exit(&output, 0);
    let info: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(info["name"], "pack");
    assert_eq!(info["info_hash"], seeder.info_hash.as_str());
    assert_eq!(info["total"], 400 * 1024);
    let files = info["files"].as_array().unwrap();
    assert_eq!(files.len(), 2);
    assert_eq!(files[0]["path"], "a.bin");
    assert_eq!(files[1]["path"], "dir/b.bin");
    assert_eq!(files[1]["size"], 100 * 1024);

    let output = fx.run(&["torrent-files", torrent.to_str().unwrap()]).await;
    assert_exit(&output, 0);
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("pack"), "{text}");
    assert!(text.contains("300.0 KiB  a.bin"), "{text}");
    assert!(text.contains("100.0 KiB  dir/b.bin"), "{text}");
    // Looking never adds a job.
    let list = fx.run(&["list"]).await;
    assert_eq!(String::from_utf8_lossy(&list.stdout), "No downloads.\n");
}

#[tokio::test(flavor = "multi_thread")]
async fn files_option_requires_single_torrent() {
    let fx = Fixture::new().await;
    let magnet = "magnet:?xt=urn:btih:c12fe1c06bba254a9dc9f519b335aa7c1367a88a";
    let other = "magnet:?xt=urn:btih:d12fe1c06bba254a9dc9f519b335aa7c1367a88a";

    let two = fx.run(&["add", magnet, other, "--files", "0"]).await;
    assert_exit(&two, 1);
    assert!(String::from_utf8_lossy(&two.stderr).contains("--files"));
    let http = fx
        .run(&["add", "http://127.0.0.1:9/x.bin", "--files", "0"])
        .await;
    assert_exit(&http, 1);
    assert!(String::from_utf8_lossy(&http.stderr).contains("--files"));
    for bad in ["", "a", "3-1", "1,,2", "-2"] {
        let output = fx.run(&["add", magnet, "--files", bad]).await;
        assert_exit(&output, 1);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn config_torrent_roundtrip() {
    let fx = Fixture::new().await;

    assert_exit(
        &fx.run(&["config", "set", "torrent.ratio", "never"]).await,
        0,
    );
    assert_exit(
        &fx.run(&["config", "set", "torrent.seed_time", "2h"]).await,
        0,
    );
    assert_exit(
        &fx.run(&["config", "set", "torrent.upload_limit", "1MB/s"])
            .await,
        0,
    );
    let port = fx.run(&["config", "set", "torrent.port", "51413"]).await;
    assert_exit(&port, 0);
    assert!(
        String::from_utf8_lossy(&port.stdout).contains("applies next time Taurus starts"),
        "stdout: {}",
        String::from_utf8_lossy(&port.stdout)
    );

    let peers = fx.run(&["config", "set", "torrent.peers", "80"]).await;
    assert_exit(&peers, 0);
    assert!(String::from_utf8_lossy(&peers.stdout).contains("applies next time Taurus starts"));

    let output = fx.run(&["--json", "config", "get"]).await;
    assert_exit(&output, 0);
    let settings: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let torrent = &settings["torrent"];
    assert_eq!(torrent["stop_ratio"], serde_json::Value::Null);
    assert_eq!(torrent["stop_after_secs"], 7200);
    assert_eq!(torrent["upload_limit_bps"], 1_000_000);
    assert_eq!(torrent["listen_port"], 51413);

    let output = fx.run(&["config", "get", "torrent.seed_time"]).await;
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "torrent.seed_time = 2h\n"
    );
    let output = fx.run(&["--json", "config", "get", "torrent.ratio"]).await;
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        r#"{"torrent.ratio":null}"#
    );
    let output = fx.run(&["config", "get"]).await;
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("torrent.upload_limit = 1MB/s\n"), "{text}");
    assert!(text.contains("torrent.port = 51413\n"), "{text}");
}

#[tokio::test(flavor = "multi_thread")]
async fn invalid_torrent_config_exits_1() {
    let fx = Fixture::new().await;

    assert_exit(
        &fx.run(&["config", "set", "torrent.max_active", "0"]).await,
        1,
    );
    for (key, bad) in [
        ("torrent.ratio", "-1"),
        ("torrent.seed_time", "soon"),
        ("torrent.upload_limit", "fast"),
        ("torrent.port", "80"),
        ("torrent.upnp", "maybe"),
        ("torrent.peers", "0"),
    ] {
        assert_exit(&fx.run(&["config", "set", key, bad]).await, 1);
    }
}

// Video, audio and the tools command. The tools come from the testkit's fake
// host and the fake yt-dlp replays a scenario; nothing touches the network.

struct Video {
    fx: Fixture,
    host: FakeToolHost,
}

impl Video {
    /// `scenario` is what the fake yt-dlp replays (see `media_scenario`).
    async fn new(scenario: &Value) -> Video {
        let version = unique_version();
        set_ytdlp_scenario(&version, &scenario.to_string());
        let host = FakeToolHost::start(
            ToolRelease::new(&version),
            ToolRelease::new("7.1"),
            ToolRelease::new("2.5.0"),
        )
        .await;
        let mut fx = Fixture::new().await;
        fx.tool_base = Some(host.base_url());
        Video { fx, host }
    }

    async fn tools(&self) -> Vec<Value> {
        let output = self.fx.run(&["--json", "tools", "status"]).await;
        assert_exit(&output, 0);
        serde_json::from_slice(&output.stdout).expect("tools status --json prints an array")
    }
}

fn core_fixture(name: &str) -> Value {
    let path = format!(
        "{}/../taurus-core/tests/fixtures/ytdlp/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
}

/// The video's info without its thumbnail (which would be fetched).
fn video_info() -> Value {
    let mut info = core_fixture("single_video.json");
    info.as_object_mut().unwrap().remove("thumbnail");
    info
}

/// Inspection (`-J`) prints `info`; every other run replays `download`.
fn media_scenario(info: &Value, download: Value) -> Value {
    json!([{ "if_arg": "-J", "stdout": [info.to_string()] }, download])
}

/// A download that reports its progress and file, and writes it.
fn downloads(name: &str, size: u64) -> Value {
    let progress = json!({
        "status": "downloading", "downloaded_bytes": size, "total_bytes": size,
        "speed": 1000.0, "filename": name,
    });
    json!({
        "stdout": [format!("TAURUS-PROGRESS {progress}"), format!("TAURUS-FILE {}", json!(name))],
        "write_files": [[name, size]],
    })
}

/// yt-dlp's download runs (not inspections) that were given `link`.
fn download_argvs(link: &str) -> Vec<Vec<String>> {
    process_args_log()
        .into_iter()
        .filter(|e| e.tool == "yt-dlp" && e.argv.iter().any(|a| a == link))
        .filter(|e| !e.argv.iter().any(|a| a == "-J"))
        .map(|e| e.argv)
        .collect()
}

/// Kills the fake yt-dlp download runs given `link`: one that a `taurus`
/// killed outright leaves behind would wait for an interrupt for minutes
/// (on Windows the killed process's job object ends it).
fn kill_orphaned_downloads(link: &str) {
    for entry in process_args_log() {
        let download = entry.tool == "yt-dlp"
            && entry.argv.iter().any(|a| a == link)
            && !entry.argv.iter().any(|a| a == "-J");
        if cfg!(unix) && download {
            let _ = std::process::Command::new("kill")
                .args(["-KILL", &entry.pid.to_string()])
                .status();
        }
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[tokio::test(flavor = "multi_thread")]
async fn add_video_with_audio_flag() {
    let name = "A Fine Video [abc123].mp3";
    let video = Video::new(&media_scenario(&video_info(), downloads(name, 2048))).await;
    let link = "https://www.youtube.com/watch?v=cli_audio_flag";

    let output = video
        .fx
        .run(&[
            "add",
            link,
            "--audio",
            "--dir",
            video.fx.out.to_str().unwrap(),
        ])
        .await;

    assert_exit(&output, 0);
    let runs = download_argvs(link);
    assert_eq!(runs.len(), 1, "{runs:?}");
    let argv = &runs[0];
    assert!(argv.iter().any(|a| a == "-x"), "{argv:?}");
    assert!(argv.windows(2).any(|w| w == ["-f", "ba/b"]), "{argv:?}");
    assert_eq!(fs::metadata(video.fx.out.join(name)).unwrap().len(), 2048);
    let list = video.fx.run(&["--json", "list"]).await;
    let jobs: Value = serde_json::from_slice(&list.stdout).unwrap();
    assert_eq!(jobs[0]["kind"], "Media");
    assert_eq!(jobs[0]["state"], "Completed");
}

#[tokio::test(flavor = "multi_thread")]
async fn unavailable_quality_falls_back_to_best() {
    // An audio-only source lists no video qualities at all.
    let mut audio_only = video_info();
    audio_only["formats"] = json!([
        {"format_id": "140", "ext": "m4a", "vcodec": "none", "acodec": "mp4a", "abr": 128}
    ]);
    let video = Video::new(&media_scenario(&audio_only, downloads("v.mp4", 64))).await;
    let link = "https://www.youtube.com/watch?v=cli_quality_fallback";
    let dir = video.fx.out.to_str().unwrap();

    let output = video
        .fx
        .run(&["add", link, "--quality", "720p", "--dir", dir])
        .await;

    assert_exit(&output, 0);
    let err = stderr(&output);
    assert!(err.contains("720p is not available"), "{err}");
    let runs = download_argvs(link);
    assert_eq!(runs.len(), 1);
    assert!(
        runs[0].windows(2).any(|w| w == ["-f", "bv*+ba/b"]),
        "{:?}",
        runs[0]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn tools_status_json() {
    let video = Video::new(&media_scenario(&video_info(), downloads("v.mp4", 64))).await;
    let before = video.tools().await;
    assert_eq!(before.len(), 3);
    assert!(before.iter().all(|t| t["state"] == "Missing"), "{before:?}");

    assert_exit(&video.fx.run(&["tools", "install"]).await, 0);

    let after = video.tools().await;
    assert_eq!(after.len(), 3);
    for tool in &after {
        assert_eq!(tool["state"], "Ready", "{tool}");
        assert!(
            tool["version"].is_string() && tool["path"].is_string(),
            "{tool}"
        );
    }
    let text = video.fx.run(&["tools"]).await;
    assert_exit(&text, 0);
    let text = String::from_utf8_lossy(&text.stdout).into_owned();
    for name in ["yt-dlp", "ffmpeg", "deno"] {
        assert!(text.contains(name), "{text}");
    }
    assert!(text.contains("Ready"), "{text}");
}

#[tokio::test(flavor = "multi_thread")]
async fn tools_rollback() {
    let video = Video::new(&media_scenario(&video_info(), downloads("v.mp4", 64))).await;
    assert_exit(&video.fx.run(&["tools", "install"]).await, 0);
    let ytdlp = |tools: &[Value]| {
        tools.iter().find(|t| t["kind"] == "YtDlp").unwrap()["version"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    let first = ytdlp(&video.tools().await);

    // Nothing to go back to yet.
    let output = video.fx.run(&["tools", "rollback", "yt-dlp"]).await;
    assert_exit(&output, 1);

    let newer = unique_version();
    video.host.set_release("yt-dlp", ToolRelease::new(&newer));
    assert_exit(&video.fx.run(&["tools", "update", "--force"]).await, 0);
    assert_eq!(ytdlp(&video.tools().await), newer);

    let output = video.fx.run(&["tools", "rollback", "yt-dlp"]).await;
    assert_exit(&output, 0);
    assert!(String::from_utf8_lossy(&output.stdout).contains(&first));
    assert_eq!(ytdlp(&video.tools().await), first);
}

#[tokio::test(flavor = "multi_thread")]
async fn large_playlist_needs_yes() {
    let entries: Vec<Value> = (0..51)
        .map(|i| json!({"_type": "url", "id": format!("v{i}"), "title": format!("V{i}")}))
        .collect();
    let info = json!({"_type": "playlist", "id": "PL51", "title": "Big", "entries": entries});
    let video = Video::new(&media_scenario(
        &info,
        downloads("Big/001 - V0 [v0].mp4", 16),
    ))
    .await;
    let link = "https://www.youtube.com/playlist?list=PLcli_large";
    let dir = video.fx.out.to_str().unwrap();

    let output = video.fx.run(&["add", link, "--dir", dir]).await;

    assert_exit(&output, 1);
    assert!(
        stderr(&output)
            .contains("This playlist has 51 videos. Run again with --yes to download all of them."),
        "{}",
        stderr(&output)
    );
    let list = video.fx.run(&["--json", "list"]).await;
    assert_eq!(
        serde_json::from_slice::<Value>(&list.stdout).unwrap(),
        json!([])
    );

    assert_exit(
        &video.fx.run(&["add", link, "--yes", "--dir", dir]).await,
        0,
    );
    let list = video.fx.run(&["--json", "list"]).await;
    let jobs: Value = serde_json::from_slice(&list.stdout).unwrap();
    assert_eq!(jobs[0]["state"], "Completed");
}

/// A live stream that records until it is stopped, then leaves a partial
/// file that the fake ffmpeg turns into the final one. As real yt-dlp
/// records a live stream (through ffmpeg): the entry with the file it goes
/// to, then no progress at all.
fn live_recording() -> Value {
    let part = "Live Now [live1].mp4.part";
    json!({
        "stdout": [format!(
            "TAURUS-ENTRY 0 1 {} {} {}",
            json!("live1"),
            json!("Live Now"),
            json!("Live Now [live1].mp4")
        )],
        "start_files": [[part, 4096]],
        "wait_for_interrupt": true,
        "on_interrupt": { "write_files": [["interrupted", 1]], "exit": 1 },
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn stop_recording_command() {
    tokio::time::timeout(TIMEOUT, stop_recording())
        .await
        .expect("stop did not finish in 60 s");
}

async fn stop_recording() {
    let video = Video::new(&media_scenario(
        &core_fixture("live.json"),
        live_recording(),
    ))
    .await;
    let link = "https://www.twitch.tv/cli_stop_recording";
    let mut child = video
        .fx
        .taurus()
        .args([
            "--json",
            "add",
            link,
            "--dir",
            video.fx.out.to_str().unwrap(),
        ])
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn taurus");
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    loop {
        let line = lines
            .next_line()
            .await
            .unwrap()
            .expect("taurus ended before recording");
        if line.contains("\"Progress\"") {
            break;
        }
    }

    // A second process asks the running one to stop.
    let stop = video.fx.run(&["stop", "1"]).await;
    assert_exit(&stop, 0);
    assert!(String::from_utf8_lossy(&stop.stdout).contains("Live Now [live1].mp4"));

    while lines.next_line().await.unwrap().is_some() {}
    let status = child.wait().await.unwrap();
    assert_eq!(status.code(), Some(0));
    assert_eq!(
        fs::metadata(video.fx.out.join("Live Now [live1].mp4"))
            .unwrap()
            .len(),
        4096
    );
    let list = video.fx.run(&["--json", "list"]).await;
    let jobs: Value = serde_json::from_slice(&list.stdout).unwrap();
    assert_eq!(jobs[0]["state"], "Completed");

    // Nothing records now: stopping again says so.
    let again = video.fx.run(&["stop", "1"]).await;
    assert_exit(&again, 1);
    assert!(
        stderr(&again).contains("not recording"),
        "{}",
        stderr(&again)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn meaningless_video_flags_exit_1() {
    let fx = Fixture::new().await;
    for args in [
        ["add", "http://127.0.0.1:9/x", "--quality", "small"],
        ["add", "http://127.0.0.1:9/x", "--audio", "--quality=720p"],
    ] {
        let output = fx.run(&args).await;
        assert_exit(&output, 1);
        assert!(stderr(&output).contains("--quality") || stderr(&output).contains("--audio"));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn convert_flag_reaches_yt_dlp() {
    let name = "A Fine Video [abc123].mp3";
    let video = Video::new(&media_scenario(&video_info(), downloads(name, 64))).await;
    let link = "https://www.youtube.com/watch?v=cli_convert_mp3";
    let dir = video.fx.out.to_str().unwrap();

    assert_exit(
        &video
            .fx
            .run(&["add", link, "--convert", "mp3", "--dir", dir])
            .await,
        0,
    );

    let runs = download_argvs(link);
    assert_eq!(runs.len(), 1);
    assert!(
        runs[0]
            .windows(3)
            .any(|w| w == ["-x", "--audio-format", "mp3"]),
        "{:?}",
        runs[0]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn web_page_that_yt_dlp_reads_is_added_as_media() {
    let name = "A Fine Video [abc123].mp4";
    let video = Video::new(&media_scenario(&video_info(), downloads(name, 64))).await;
    let page = video.fx.server.serve_page(
        "/videos/cli_page_falls_back",
        200,
        &[],
        "<html><body><video src=x></video></body></html>",
    );

    let output = video
        .fx
        .run(&["add", &page, "--dir", video.fx.out.to_str().unwrap()])
        .await;

    assert_exit(&output, 0);
    assert_eq!(download_argvs(&page).len(), 1);
    let list = video.fx.run(&["--json", "list"]).await;
    let jobs: Value = serde_json::from_slice(&list.stdout).unwrap();
    assert_eq!(jobs[0]["kind"], "Media");
    assert_eq!(jobs[0]["state"], "Completed");
}

#[tokio::test(flavor = "multi_thread")]
async fn video_flags_on_a_plain_file_are_ignored_with_a_note() {
    let video = Video::new(&media_scenario(&video_info(), downloads("v.mp4", 64))).await;
    let spec = FileSpec::new(MIB);
    let url = video.fx.server.serve(PATH, spec.clone());
    let dir = video.fx.out.to_str().unwrap();

    let output = video.fx.run(&["add", &url, "--audio", "--dir", dir]).await;

    assert_exit(&output, 0);
    assert!(
        stderr(&output).contains("not a video link"),
        "{}",
        stderr(&output)
    );
    assert_same_bytes(&video.fx.file(), &spec);
}

/// Holds the data dir's lock as a process that never reads stop requests.
fn hold_lock(fx: &Fixture) -> fs::File {
    let data = fx.tmp.path().join("data");
    fs::create_dir_all(&data).unwrap();
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(data.join("taurus.lock"))
        .unwrap();
    file.try_lock().expect("the lock is free");
    file
}

#[tokio::test(flavor = "multi_thread")]
async fn unanswered_stop_exits_1_and_withdraws_the_request() {
    let video = Video::new(&media_scenario(
        &core_fixture("live.json"),
        live_recording(),
    ))
    .await;
    let link = "https://www.twitch.tv/cli_unanswered_stop";
    let mut child = video
        .fx
        .taurus()
        .args([
            "--json",
            "add",
            link,
            "--dir",
            video.fx.out.to_str().unwrap(),
        ])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    loop {
        let line = lines.next_line().await.unwrap().expect("no progress");
        if line.contains("\"Progress\"") {
            break;
        }
    }
    // Killed: the store still says Recording, as after a crash. Then
    // something else holds the data dir and never answers.
    child.kill().await.unwrap();
    kill_orphaned_downloads(link);
    let _lock = hold_lock(&video.fx);

    let started = std::time::Instant::now();
    let stop = video.fx.run(&["stop", "1"]).await;

    assert_exit(&stop, 1);
    assert!(started.elapsed() < Duration::from_secs(30));
    let err = stderr(&stop);
    assert!(err.contains("did not answer"), "{err}");
    assert!(!video.fx.tmp.path().join("data/stop-requests/1").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn stop_names_what_is_wrong() {
    let fx = Fixture::new().await;
    let url = fx.server.serve(PATH, FileSpec::new(MIB));
    assert_exit(&fx.add(&url, &fx.out).await, 0);

    // Nothing holds the data dir.
    for (id, words) in [("1", "not a video"), ("42", "not found")] {
        let output = fx.run(&["stop", id]).await;
        assert_exit(&output, 1);
        assert!(stderr(&output).contains(words), "{}", stderr(&output));
    }
    // Something does: the same answers, without waiting for it.
    let _lock = hold_lock(&fx);
    for (id, words) in [("1", "not a video"), ("42", "not found")] {
        let output = fx.run(&["stop", id]).await;
        assert_exit(&output, 1);
        assert!(stderr(&output).contains(words), "{}", stderr(&output));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn tools_status_works_while_the_data_dir_is_held() {
    let video = Video::new(&media_scenario(&video_info(), downloads("v.mp4", 64))).await;
    assert_exit(&video.fx.run(&["tools", "install"]).await, 0);
    let _lock = hold_lock(&video.fx);

    let tools = video.tools().await;

    assert_eq!(tools.len(), 3);
    assert!(tools.iter().all(|t| t["state"] == "Ready"), "{tools:?}");
    // The rest of the command needs the engine.
    assert_exit(&video.fx.run(&["tools", "install"]).await, 1);
}

// Grab, passwords, video options, convert, torrent selection and the proxy.

const PAGE: &str = "/files/index.html";

/// A page linking two PDFs and a ZIP, which the server also serves.
fn serve_files_page(fx: &Fixture) -> (String, FileSpec, FileSpec) {
    let (pdf, zip) = (FileSpec::new(2000), FileSpec::new(3000));
    fx.server.serve("/files/a.pdf", pdf.clone());
    fx.server.serve("/files/b.zip", zip.clone());
    fx.server.serve(
        "/files/c.pdf",
        FileSpec {
            seed: 7,
            ..FileSpec::new(4000)
        },
    );
    let html = r#"<html><body>
        <a href="a.pdf">A</a> <a href="b.zip">B</a> <a href="c.pdf">C</a>
        <a href="/about.html">About</a>
        </body></html>"#;
    let page = fx.server.serve_page(PAGE, 200, &[], html);
    (page, pdf, zip)
}

#[tokio::test(flavor = "multi_thread")]
async fn grab_lists_and_adds() {
    let fx = Fixture::new().await;
    let (page, pdf, _) = serve_files_page(&fx);
    let dir = fx.out.to_str().unwrap();

    // Everything linked that is a file, listed without asking for sizes.
    let all = fx.run(&["grab", &page]).await;
    assert_exit(&all, 0);
    let text = String::from_utf8_lossy(&all.stdout);
    for name in ["a.pdf", "b.zip", "c.pdf"] {
        assert!(text.contains(name), "{text}");
    }
    assert!(!text.contains("about.html"), "{text}");
    assert!(!text.contains("SIZE"), "{text}");
    assert_eq!(fx.server.requests("/files/a.pdf"), 0);

    // --ext narrows it; --json gives index, folder and name.
    let output = fx.run(&["--json", "grab", &page, "--ext", "PDF"]).await;
    assert_exit(&output, 0);
    let grabbed: Value = serde_json::from_slice(&output.stdout).unwrap();
    let rows = grabbed["links"].as_array().unwrap();
    let names: Vec<&str> = rows.iter().map(|r| r["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["a.pdf", "c.pdf"]);
    assert_eq!(rows[0]["index"], 0);
    assert_eq!(rows[1]["index"], 1);
    assert_eq!(rows[0].get("size"), None);
    assert_eq!(rows[0]["folder"], "/");
    assert_eq!(grabbed["truncated"], false);

    // --sizes: after the listing, one line per size as it arrives, by index.
    let output = fx.run(&["grab", &page, "--ext", "pdf", "--sizes"]).await;
    assert_exit(&output, 0);
    let text = String::from_utf8_lossy(&output.stdout);
    let (listing, sizes) = text.split_once("INDEX        SIZE\n").expect("sizes");
    assert!(
        listing.contains("a.pdf") && listing.contains("c.pdf"),
        "{text}"
    );
    let mut lines: Vec<&str> = sizes.lines().collect();
    lines.sort();
    assert_eq!(lines, ["    0     2.0 KiB", "    1     3.9 KiB"], "{text}");
    // With --json, one object per size after the listing.
    let output = fx
        .run(&["--json", "grab", &page, "--ext", "pdf", "--sizes"])
        .await;
    assert_exit(&output, 0);
    let values: Vec<Value> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(values.len(), 3, "{values:?}");
    assert_eq!(values[0]["links"].as_array().unwrap().len(), 2);
    let mut sizes: Vec<(u64, u64)> = values[1..]
        .iter()
        .map(|v| (v["index"].as_u64().unwrap(), v["size"].as_u64().unwrap()))
        .collect();
    sizes.sort();
    assert_eq!(sizes, [(0, 2000), (1, 4000)]);
    // --add never waits on sizes: the two do not go together.
    assert_exit(
        &fx.run(&["grab", &page, "--sizes", "--add", "--dir", dir])
            .await,
        1,
    );

    // Listing alone downloads nothing and queues nothing.
    assert!(fs::read_dir(&fx.out).unwrap().next().is_none());
    let list = fx.run(&["--json", "list"]).await;
    assert_eq!(String::from_utf8_lossy(&list.stdout).trim(), "[]");

    // --add downloads exactly what is listed.
    let output = fx
        .run(&["grab", &page, "--ext", "pdf", "--add", "--dir", dir])
        .await;
    assert_exit(&output, 0);
    assert_same_bytes(&fx.out.join("a.pdf"), &pdf);
    assert_eq!(fs::metadata(fx.out.join("c.pdf")).unwrap().len(), 4000);
    assert!(!fx.out.join("b.zip").exists());
    let list = fx.run(&["--json", "list"]).await;
    let jobs: Value = serde_json::from_slice(&list.stdout).unwrap();
    assert_eq!(jobs.as_array().unwrap().len(), 2);

    // Nothing matches: say so, exit 0.
    let output = fx.run(&["grab", &page, "--ext", "iso"]).await;
    assert_exit(&output, 0);
    assert!(String::from_utf8_lossy(&output.stdout).contains("No files found."));
    // A page that is not there fails.
    assert_exit(&fx.run(&["grab", "http://127.0.0.1:9/none"]).await, 1);
    // --dir means nothing without --add.
    assert_exit(&fx.run(&["grab", &page, "--dir", dir]).await, 1);
}

fn listing_page(title: &str, links: &[&str]) -> String {
    let rows: String = links
        .iter()
        .map(|l| format!("<a href=\"{l}\">{l}</a>\n"))
        .collect();
    format!(
        "<html><head><title>{title}</title></head><body><h1>{title}</h1><pre>{rows}</pre></body></html>"
    )
}

fn header(server: &TestServer, path: &str, name: &str) -> Option<String> {
    server
        .last_request_headers(path)
        .into_iter()
        .find(|(n, _)| n == name)
        .map(|(_, v)| v)
}

#[tokio::test(flavor = "multi_thread")]
async fn grab_add_keeps_subfolders_and_sends_the_page_as_referer() {
    let fx = Fixture::new().await;
    let other = TestServer::start().await;
    let (a, b) = (
        FileSpec::new(1500),
        FileSpec {
            seed: 5,
            ..FileSpec::new(2500)
        },
    );
    fx.server.serve("/lst/a/x.pdf", a.clone());
    fx.server.serve("/lst/b/x.pdf", b.clone());
    fx.server.serve("/lst/r.pdf", FileSpec::new(700));
    let far = other.serve("/far.pdf", FileSpec::new(900));
    let page = fx.server.serve_page(
        "/lst/",
        200,
        &[],
        &listing_page("Index of /lst/", &["a/", "b/", "r.pdf", &far]),
    );
    fx.server.serve_page(
        "/lst/a/",
        200,
        &[],
        &listing_page("Index of /lst/a/", &["x.pdf"]),
    );
    fx.server.serve_page(
        "/lst/b/",
        200,
        &[],
        &listing_page("Index of /lst/b/", &["x.pdf"]),
    );

    let dir = fx.out.to_str().unwrap();
    let output = fx
        .run(&["grab", &page, "--subfolders", "--add", "--dir", dir])
        .await;

    assert_exit(&output, 0);
    // Same names in different folders stay apart, where the listing put them.
    assert_same_bytes(&fx.out.join("a/x.pdf"), &a);
    assert_same_bytes(&fx.out.join("b/x.pdf"), &b);
    assert_eq!(fs::metadata(fx.out.join("r.pdf")).unwrap().len(), 700);
    assert!(!fx.out.join("x.pdf").exists());
    assert!(!fx.out.join("x (1).pdf").exists());
    // The page is the Referer for its own host's files, and for no other host.
    assert_eq!(
        header(&fx.server, "/lst/r.pdf", "referer").as_deref(),
        Some(page.as_str())
    );
    assert_eq!(header(&other, "/far.pdf", "referer"), None);
    assert_eq!(fs::metadata(fx.out.join("far.pdf")).unwrap().len(), 900);
}

#[tokio::test(flavor = "multi_thread")]
async fn grab_add_keeps_hostile_folders_inside_dir() {
    let fx = Fixture::new().await;
    // The server decodes the path of a request, so these are registered
    // decoded: the links name them as `x%2F..%2F..%2Fescape/` and `a%5Cb/`.
    for folder in ["/h/x/../../escape/", "/h/a\\b/", "/h/ok/"] {
        fx.server
            .serve(&format!("{folder}ev.pdf"), FileSpec::new(300));
        fx.server.serve_page(
            folder,
            200,
            &[],
            &listing_page("Index of folder", &["ev.pdf"]),
        );
    }
    let page = fx.server.serve_page(
        "/h/",
        200,
        &[],
        &listing_page("Index of /h/", &["x%2F..%2F..%2Fescape/", "a%5Cb/", "ok/"]),
    );
    let sandbox = fx.tmp.path().join("sandbox");
    fs::create_dir(&sandbox).unwrap();
    let dir = sandbox.join("dl");
    fs::create_dir(&dir).unwrap();

    let output = fx
        .run(&[
            "grab",
            &page,
            "--subfolders",
            "--add",
            "--dir",
            dir.to_str().unwrap(),
        ])
        .await;

    // The unsafe folders fail; the good one downloads.
    assert_exit(&output, 1);
    assert!(stderr(&output).contains("folder"), "{}", stderr(&output));
    assert_eq!(fs::metadata(dir.join("ok/ev.pdf")).unwrap().len(), 300);
    let mut inside = Vec::new();
    let mut todo = vec![sandbox.clone()];
    while let Some(next) = todo.pop() {
        for entry in fs::read_dir(next).unwrap().flatten() {
            if entry.path().is_dir() {
                todo.push(entry.path());
            } else {
                inside.push(entry.path());
            }
        }
    }
    assert_eq!(inside, [dir.join("ok/ev.pdf")], "{inside:?}");
    // Nothing next to the sandbox or the destination either.
    let entries = |p: &Path| fs::read_dir(p).unwrap().count();
    assert_eq!(entries(&sandbox), 1);
    assert_eq!(entries(&dir), 1);
}

const SECRET: &str = "s3cret-pw-4711";

/// The names of the files under `dir` whose bytes contain `needle`.
fn files_containing(dir: &Path, needle: &str) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut todo = vec![dir.to_path_buf()];
    while let Some(next) = todo.pop() {
        for entry in fs::read_dir(&next).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                todo.push(path);
            } else if fs::read(&path)
                .is_ok_and(|b| b.windows(needle.len()).any(|w| w == needle.as_bytes()))
            {
                found.push(path);
            }
        }
    }
    found
}

fn protected(fx: &Fixture, spec: FileSpec) -> String {
    let spec = FileSpec {
        basic_auth: Some(("bob".to_string(), SECRET.to_string())),
        ..spec
    };
    fx.server.serve(PATH, spec)
}

#[tokio::test(flavor = "multi_thread")]
async fn add_with_user_prompts() {
    let fx = Fixture::new().await;
    let spec = FileSpec::new(5000);
    let url = protected(&fx, spec.clone());
    let dir = fx.out.to_str().unwrap();
    let say = |o: &Output| format!("{}{}", String::from_utf8_lossy(&o.stdout), stderr(o));

    // No terminal and no test password: it says so and asks for nothing else.
    let output = fx.run(&["add", &url, "--user", "bob", "--dir", dir]).await;
    assert_exit(&output, 1);
    assert!(stderr(&output).contains("terminal"), "{}", stderr(&output));
    assert!(!fx.file().exists());

    // A user name that carries a password is refused, and not repeated.
    let output = fx
        .run(&["add", &url, "--user", "bob:tryme", "--dir", dir])
        .await;
    assert_exit(&output, 1);
    assert!(!say(&output).contains("tryme"), "{}", say(&output));

    // A password is never an option.
    let output = fx
        .run(&["add", &url, "--user", "bob", "--password", SECRET])
        .await;
    assert_exit(&output, 1);
    assert!(!say(&output).contains(SECRET));

    // A wrong password fails and is not repeated.
    let mut cmd = fx.taurus();
    cmd.env("TAURUS_PASSWORD", "wrong-password")
        .args(["add", &url, "--user", "bob", "--dir", dir]);
    let output = output_of(cmd).await;
    assert_exit(&output, 1);
    assert!(say(&output).contains("wrong user name or password"));
    assert!(!say(&output).contains("wrong-password"));
    assert!(!fx.file().exists());

    // The right one downloads the protected file.
    let mut cmd = fx.taurus();
    cmd.env("TAURUS_PASSWORD", SECRET)
        .args(["--json", "add", &url, "--user", "bob", "--dir", dir]);
    let output = output_of(cmd).await;
    assert_exit(&output, 0);
    assert_same_bytes(&fx.file(), &spec);
    assert!(!say(&output).contains(SECRET));

    // The password is nowhere on disk but the file it unlocked (none here),
    // and `list` does not show it.
    let list = fx.run(&["--json", "list"]).await;
    assert!(!say(&list).contains(SECRET));
    assert!(say(&list).contains("\"state\":\"Completed\""));
    assert_eq!(
        files_containing(fx.tmp.path(), SECRET),
        Vec::<PathBuf>::new()
    );
}

/// `output` under the shorter name the new tests use.
async fn output_of(cmd: Command) -> Output {
    output(cmd).await
}

#[tokio::test(flavor = "multi_thread")]
async fn login_resumes_a_job_that_needs_sign_in() {
    tokio::time::timeout(TIMEOUT, login_after_restart())
        .await
        .expect("login did not finish in 60 s");
}

async fn login_after_restart() {
    let fx = Fixture::new().await;
    // 64 MiB at 2 MiB/s: the download cannot finish in the ~30 s it would
    // take, so the kill after 4 MiB always lands while it runs.
    let mut spec = FileSpec::new(64 * MIB);
    spec.bytes_per_sec = Some(2 * MIB as u64);
    let url = protected(&fx, spec.clone());
    let mut child = {
        let mut cmd = fx.taurus();
        cmd.env("TAURUS_PASSWORD", SECRET).args([
            "--json",
            "add",
            &url,
            "--user",
            "bob",
            "--dir",
            fx.out.to_str().unwrap(),
        ]);
        cmd.stdout(Stdio::piped()).spawn().expect("spawn taurus")
    };
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    loop {
        let line = lines
            .next_line()
            .await
            .unwrap()
            .expect("taurus ended early");
        let event: Value = serde_json::from_str(&line).unwrap();
        if event["Progress"]["downloaded"].as_u64().unwrap_or(0) > 4 * MIB as u64 {
            break;
        }
    }
    child.kill().await.unwrap();
    let list = fx.run(&["--json", "list"]).await;
    let jobs: Value = serde_json::from_slice(&list.stdout).unwrap();
    assert_ne!(
        jobs[0]["state"], "Completed",
        "finished before the kill: {jobs}"
    );
    // The rest comes at full speed (same bytes, still protected), so the
    // test stays short.
    fx.server.replace(
        PATH,
        FileSpec {
            basic_auth: Some(("bob".to_string(), SECRET.to_string())),
            bytes_per_sec: None,
            ..spec.clone()
        },
    );

    // The password lived in memory only: the next run cannot sign in.
    let output = fx.run(&["run"]).await;
    assert_exit(&output, 1);
    let list = fx.run(&["--json", "list"]).await;
    let jobs: Value = serde_json::from_slice(&list.stdout).unwrap();
    assert_eq!(jobs[0]["state"], "Failed", "{jobs}");
    assert_eq!(jobs[0]["error"][0], "NeedsSignIn", "{jobs}");

    // A wrong password stays failed; the right one finishes the file.
    let login = |password: &str| {
        let mut cmd = fx.taurus();
        cmd.env("TAURUS_PASSWORD", password)
            .args(["login", "1", "--user", "bob"]);
        cmd
    };
    let output = output_of(login("nope")).await;
    assert_exit(&output, 1);
    assert!(!stderr(&output).contains("nope"));
    let output = output_of(login(SECRET)).await;
    assert_exit(&output, 0);
    assert_same_bytes(&fx.file(), &spec);
    assert_exit(&fx.run(&["login", "9", "--user", "bob"]).await, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn video_options_reach_yt_dlp() {
    let name = "A Fine Video [abc123].mkv";
    let video = Video::new(&media_scenario(&video_info(), downloads(name, 64))).await;
    let link = "https://www.youtube.com/watch?v=cli_video_options";
    let dir = video.fx.out.to_str().unwrap();

    let output = video
        .fx
        .run(&[
            "add",
            link,
            "--cookies-from",
            "firefox:work",
            "--subs",
            "en,vi",
            "--auto-subs",
            "--embed-subs",
            "--container",
            "mkv",
            "--dir",
            dir,
        ])
        .await;

    assert_exit(&output, 0);
    let runs = download_argvs(link);
    assert_eq!(runs.len(), 1, "{runs:?}");
    let argv = &runs[0];
    let has = |pair: &[&str]| argv.windows(pair.len()).any(|w| w == pair);
    assert!(has(&["--cookies-from-browser", "firefox:work"]), "{argv:?}");
    assert!(argv.iter().any(|a| a == "--write-subs"), "{argv:?}");
    assert!(argv.iter().any(|a| a == "--write-auto-subs"), "{argv:?}");
    assert!(argv.iter().any(|a| a == "--embed-subs"), "{argv:?}");
    assert!(has(&["--sub-langs", "en,vi"]), "{argv:?}");
    assert!(has(&["--merge-output-format", "mkv"]), "{argv:?}");

    // Values that mean nothing exit 1 before anything runs.
    for args in [
        &["add", link, "--cookies-from", "netscape"][..],
        &["add", link, "--auto-subs"],
        &["add", link, "--container", "avi"],
        &[
            "add",
            link,
            "--convert",
            "smaller-mp4",
            "--container",
            "webm",
        ],
    ] {
        assert_exit(&video.fx.run(args).await, 1);
    }
    let output = video
        .fx
        .run(&["resume", "1", "--cookies-from", "netscape"])
        .await;
    assert_exit(&output, 1);
    assert!(
        stderr(&output).contains("unknown browser"),
        "{}",
        stderr(&output)
    );
}

/// The video's info with a 480p format (not one of the usual qualities),
/// subtitles and generated captions.
fn video_with_languages() -> Value {
    let mut info = video_info();
    info["formats"].as_array_mut().unwrap().push(json!({
        "format_id": "135", "ext": "mp4", "vcodec": "avc1.4d401e", "acodec": "none",
        "height": 480
    }));
    info["subtitles"] = json!({
        "vi": [{ "ext": "vtt", "url": "https://s/vi" }],
        "en": [{ "ext": "vtt", "url": "https://s/en" }],
    });
    info["automatic_captions"] = json!({ "fr": [{ "ext": "vtt", "url": "https://s/fr" }] });
    info
}

#[tokio::test(flavor = "multi_thread")]
async fn inspect_lists_every_quality_and_language() {
    let video = Video::new(&media_scenario(
        &video_with_languages(),
        downloads("v.mp4", 64),
    ))
    .await;
    let link = "https://www.youtube.com/watch?v=cli_inspect";

    let output = video.fx.run(&["inspect", link]).await;
    assert_exit(&output, 0);
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("Title: A Fine Video"), "{text}");
    let qualities = text
        .lines()
        .find_map(|l| l.strip_prefix("Qualities: "))
        .unwrap_or_else(|| panic!("no qualities line: {text}"));
    let ids: Vec<&str> = qualities
        .split(", ")
        .map(|q| q.split(' ').next().unwrap())
        .collect();
    assert_eq!(
        ids,
        [
            "best",
            "2160p",
            "1080p",
            "720p",
            "480p",
            "audio-best",
            "audio-small"
        ],
        "{text}"
    );
    assert!(text.contains("Subtitles: en, vi\n"), "{text}");
    assert!(text.contains("Auto captions: fr\n"), "{text}");

    let output = video
        .fx
        .run(&["--json", "inspect", link, "--cookies-from", "firefox"])
        .await;
    assert_exit(&output, 0);
    let info: Value = serde_json::from_slice(&output.stdout).unwrap();
    let media = &info["Media"];
    assert_eq!(media["title"], "A Fine Video", "{info}");
    let heights: Vec<&str> = media["all_qualities"]
        .as_array()
        .unwrap()
        .iter()
        .map(|q| q["id"].as_str().unwrap())
        .collect();
    assert_eq!(heights, ["2160p", "1080p", "720p", "480p"]);
    assert_eq!(media["subtitle_languages"], json!(["en", "vi"]));
    assert_eq!(media["auto_caption_languages"], json!(["fr"]));
    // The cookies reached the inspection.
    let inspections: Vec<Vec<String>> = process_args_log()
        .into_iter()
        .filter(|e| e.tool == "yt-dlp" && e.argv.iter().any(|a| a == link))
        .map(|e| e.argv)
        .collect();
    assert!(
        inspections.iter().any(|argv| argv
            .windows(2)
            .any(|w| w == ["--cookies-from-browser", "firefox"])),
        "{inspections:?}"
    );

    // Nothing was added, and nothing was downloaded.
    let list = video.fx.run(&["--json", "list"]).await;
    assert_eq!(
        serde_json::from_slice::<Value>(&list.stdout).unwrap(),
        json!([])
    );
    assert!(download_argvs(link).is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn any_height_reaches_yt_dlp() {
    let name = "A Fine Video [abc123].mp4";
    let video = Video::new(&media_scenario(
        &video_with_languages(),
        downloads(name, 64),
    ))
    .await;
    let link = "https://www.youtube.com/watch?v=cli_quality_480p";
    let dir = video.fx.out.to_str().unwrap();

    let output = video
        .fx
        .run(&["add", link, "--quality", "480p", "--dir", dir])
        .await;

    assert_exit(&output, 0);
    assert!(
        !stderr(&output).contains("not available"),
        "{}",
        stderr(&output)
    );
    let runs = download_argvs(link);
    assert_eq!(runs.len(), 1, "{runs:?}");
    assert!(
        runs[0]
            .windows(2)
            .any(|w| w == ["-f", "bv*[height<=480]+ba/b[height<=480]"]),
        "{:?}",
        runs[0]
    );
    // A value that is not a quality exits 1 before anything runs.
    for bad in ["480", "hd", "0p"] {
        let output = video.fx.run(&["add", link, "--quality", bad]).await;
        assert_exit(&output, 1);
    }
    assert_eq!(download_argvs(link).len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn user_needs_exactly_one_url() {
    let fx = Fixture::new().await;
    let url = protected(&fx, FileSpec::new(5000));
    let dir = fx.out.to_str().unwrap();
    let list = fx.tmp.path().join("urls.txt");
    fs::write(&list, format!("{url}\n")).unwrap();

    for args in [
        &["add", &url, &url, "--user", "bob", "--dir", dir][..],
        &[
            "add",
            "--file",
            list.to_str().unwrap(),
            "--user",
            "bob",
            "--dir",
            dir,
        ],
        &[
            "add",
            &url,
            "--file",
            list.to_str().unwrap(),
            "--user",
            "bob",
            "--dir",
            dir,
        ],
    ] {
        let mut cmd = fx.taurus();
        cmd.env("TAURUS_PASSWORD", SECRET).args(args);
        let output = output_of(cmd).await;
        assert_exit(&output, 1);
        assert!(
            stderr(&output).contains("--user can only be used with exactly one URL"),
            "{args:?}: {}",
            stderr(&output)
        );
    }
    assert_eq!(fx.server.requests(PATH), 0);
    assert!(!fx.tmp.path().join("data").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_held_data_dir_is_reported_before_the_password_is_asked() {
    let fx = Fixture::new().await;
    let url = protected(&fx, FileSpec::new(5000));
    let _held = hold_lock(&fx);

    // No terminal and no test password: asking would fail on the terminal,
    // so the lock must be reported first.
    let output = fx
        .run(&[
            "add",
            &url,
            "--user",
            "bob",
            "--dir",
            fx.out.to_str().unwrap(),
        ])
        .await;
    assert_exit(&output, 1);
    let err = stderr(&output);
    assert!(err.contains("another taurus instance"), "{err}");
    assert!(!err.contains("terminal"), "{err}");
    let output = fx.run(&["login", "1", "--user", "bob"]).await;
    assert_exit(&output, 1);
    let err = stderr(&output);
    assert!(err.contains("another taurus instance"), "{err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn convert_command() {
    let video = Video::new(&json!([])).await;
    let fx = &video.fx;
    let dir = fx.out.to_str().unwrap();
    let song = fx.out.join("song.webm");
    fs::write(&song, vec![9u8; 4000]).unwrap();

    // A file, by path.
    let output = fx.run(&["convert", song.to_str().unwrap(), "mp3"]).await;
    assert_exit(&output, 0);
    // The fake ffmpeg copies its input; the source stays as it was.
    assert_eq!(
        fs::read(fx.out.join("song (mp3).mp3")).unwrap(),
        vec![9u8; 4000]
    );
    assert_eq!(fs::read(&song).unwrap(), vec![9u8; 4000]);

    // A finished download, by id.
    let url = fx.server.serve(PATH, FileSpec::new(3000));
    assert_exit(&fx.add(&url, &fx.out).await, 0);
    let output = fx.run(&["convert", "2", "smaller-mp4"]).await;
    assert_exit(&output, 0);
    assert_eq!(
        fs::metadata(fx.out.join("x (smaller).mp4")).unwrap().len(),
        3000
    );
    let list = fx.run(&["--json", "list"]).await;
    let jobs: Value = serde_json::from_slice(&list.stdout).unwrap();
    let kinds: Vec<&str> = jobs
        .as_array()
        .unwrap()
        .iter()
        .map(|j| j["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["Convert", "Http", "Convert"]);

    // Only web downloads take a login, said before any password is asked.
    let output = fx.run(&["login", "1", "--user", "bob"]).await;
    assert_exit(&output, 1);
    assert!(
        stderr(&output).contains("not a web download"),
        "{}",
        stderr(&output)
    );

    // Refusals say why and exit 1.
    let output = fx.run(&["convert", "77", "mp3"]).await;
    assert_exit(&output, 1);
    let output = fx
        .run(&["convert", &format!("{dir}/none.webm"), "mp3"])
        .await;
    assert_exit(&output, 1);
    assert!(
        stderr(&output).contains("not a file that exists"),
        "{}",
        stderr(&output)
    );
    assert_exit(&fx.run(&["convert", "2", "ogg"]).await, 1);
}

/// A torrent of two files, added with only the first one, and finished.
async fn finished_pack(fx: &Fixture, seeder: &TorrentSeeder) {
    assert_exit(&fx.run(&["config", "set", "torrent.ratio", "0"]).await, 0);
    let torrent = fx.tmp.path().join("pack.torrent");
    fs::write(&torrent, &seeder.torrent_bytes).unwrap();
    let mut cmd = fx.taurus_with_peer(seeder);
    cmd.args(["add", torrent.to_str().unwrap(), "--files", "0"])
        .args(["--dir", fx.out.to_str().unwrap()]);
    assert_exit(&output(cmd).await, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn torrent_select_command() {
    let fx = Fixture::new().await;
    let (seeder, _, b) = pack_seeder().await;
    finished_pack(&fx, &seeder).await;

    // Refusals change nothing and say why.
    for (list, why) in [
        ("", "empty"),
        ("1,,2", "invalid"),
        ("5-2", "backwards"),
        ("7", "no file 7"),
    ] {
        let output = fx.run(&["torrent-select", "1", list]).await;
        assert_exit(&output, 1);
        assert!(stderr(&output).contains(why), "{list}: {}", stderr(&output));
    }
    assert_exit(&fx.run(&["torrent-select", "9", "0"]).await, 1);
    let url = fx.server.serve(PATH, FileSpec::new(10));
    assert_exit(&fx.add(&url, &fx.out).await, 0);
    let output = fx.run(&["torrent-select", "2", "0"]).await;
    assert_exit(&output, 1);

    // Adding the second file queues the torrent again.
    let output = fx.run(&["torrent-select", "1", "0-1"]).await;
    assert_exit(&output, 0);
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("Download 1: 2 files selected."), "{text}");
    assert!(text.contains("taurus run"), "{text}");
    let list = fx.run(&["--json", "list"]).await;
    let jobs: Value = serde_json::from_slice(&list.stdout).unwrap();
    assert_eq!(jobs[0]["state"], "Queued");
    assert_eq!(jobs[0]["torrent"]["files_selected"], 2);

    // The next run downloads it.
    let mut cmd = fx.taurus_with_peer(&seeder);
    cmd.arg("run");
    assert_exit(&output_of(cmd).await, 0);
    assert_eq!(fs::read(fx.out.join("pack/dir/b.bin")).unwrap(), b);

    // Dropping a file keeps it on disk and the job done.
    let output = fx.run(&["--json", "torrent-select", "1", "1"]).await;
    assert_exit(&output, 0);
    let said: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(said["files"], json!([1]));
    assert_eq!(said["state"], "Completed");
}

/// The first job's own sharing rules, as `list --json` shows them.
async fn torrent_overrides(fx: &Fixture) -> Value {
    let list = fx.run(&["--json", "list"]).await;
    let jobs: Value = serde_json::from_slice(&list.stdout).unwrap();
    jobs[0]["torrent"]["overrides"].clone()
}

#[tokio::test(flavor = "multi_thread")]
async fn job_set_ratio() {
    let fx = Fixture::new().await;
    let (seeder, _, _) = pack_seeder().await;
    finished_pack(&fx, &seeder).await;

    let output = fx.run(&["job", "set", "1", "ratio", "2.5"]).await;
    assert_exit(&output, 0);
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "Download 1: ratio = 2.5\n"
    );
    assert_eq!(torrent_overrides(&fx).await, json!({"stop_ratio": 2.5}));

    // The other rule is kept when one changes.
    assert_exit(&fx.run(&["job", "set", "1", "seed_time", "2h"]).await, 0);
    assert_eq!(
        torrent_overrides(&fx).await,
        json!({"stop_ratio": 2.5, "stop_after_secs": 7200})
    );
    assert_exit(&fx.run(&["job", "set", "1", "ratio", "never"]).await, 0);
    assert_eq!(
        torrent_overrides(&fx).await,
        json!({"stop_ratio": null, "stop_after_secs": 7200})
    );

    // `default` clears one rule: it follows the global setting again.
    let output = fx.run(&["job", "set", "1", "ratio", "default"]).await;
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "Download 1: ratio = default\n"
    );
    assert_exit(
        &fx.run(&["job", "set", "1", "seed_time", "default"]).await,
        0,
    );
    assert_eq!(torrent_overrides(&fx).await, json!({}));
    assert_exit(&fx.run(&["job", "set", "1", "seed_time", "off"]).await, 0);
    assert_eq!(
        torrent_overrides(&fx).await,
        json!({"stop_after_secs": null})
    );

    // Bad values and jobs are refused.
    for args in [
        ["job", "set", "1", "ratio", "fast"],
        ["job", "set", "1", "ratio", "-1"],
        ["job", "set", "1", "seed_time", "0m"],
        ["job", "set", "1", "speed", "1"],
        ["job", "set", "5", "ratio", "1"],
    ] {
        assert_exit(&fx.run(&args).await, 1);
    }
    let url = fx.server.serve(PATH, FileSpec::new(10));
    assert_exit(&fx.add(&url, &fx.out).await, 0);
    let output = fx.run(&["job", "set", "2", "ratio", "1"]).await;
    assert_exit(&output, 1);
    assert!(
        stderr(&output).contains("not a torrent"),
        "{}",
        stderr(&output)
    );
}

async fn proxy_line(fx: &Fixture) -> String {
    let output = fx.run(&["config", "get", "network.proxy"]).await;
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[tokio::test(flavor = "multi_thread")]
async fn proxy_config_roundtrip_and_rejects_bad() {
    let fx = Fixture::new().await;
    assert_eq!(proxy_line(&fx).await, "network.proxy = system\n");

    let output = fx
        .run(&["config", "set", "network.proxy", "socks5h://127.0.0.1:9050"])
        .await;
    assert_exit(&output, 0);
    // Torrents pick up a SOCKS proxy when Taurus next starts.
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "network.proxy = socks5h://127.0.0.1:9050\napplies next time Taurus starts\n"
    );
    assert_eq!(
        proxy_line(&fx).await,
        "network.proxy = socks5h://127.0.0.1:9050\n"
    );
    let output = fx.run(&["--json", "config", "get", "network.proxy"]).await;
    let json: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        json["network.proxy"],
        json!({"Manual": {"url": "socks5h://127.0.0.1:9050"}})
    );
    assert!(
        String::from_utf8_lossy(&fx.run(&["config", "get"]).await.stdout)
            .contains("network.proxy = socks5h://127.0.0.1:9050\n")
    );

    // Leaving a SOCKS proxy says so too: a running torrent session keeps it.
    let output = fx.run(&["config", "set", "network.proxy", "none"]).await;
    assert_exit(&output, 0);
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "network.proxy = none\napplies next time Taurus starts\n"
    );
    assert_eq!(proxy_line(&fx).await, "network.proxy = none\n");
    assert_exit(
        &fx.run(&["config", "set", "network.proxy", "System"]).await,
        0,
    );
    assert_eq!(proxy_line(&fx).await, "network.proxy = system\n");
    // An http proxy is for web downloads only: used at once, no note.
    let output = fx
        .run(&[
            "config",
            "set",
            "network.proxy",
            "http://proxy.example:3128",
        ])
        .await;
    assert_exit(&output, 0);
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "network.proxy = http://proxy.example:3128\n"
    );

    // A user name or password in the URL is refused with the engine's own
    // message, which does not repeat it; the setting stays.
    let output = fx
        .run(&[
            "config",
            "set",
            "network.proxy",
            "http://amy:hunter2@proxy.example:3128",
        ])
        .await;
    assert_exit(&output, 1);
    let text = stderr(&output);
    assert!(
        text.contains("a user name or password in the proxy URL is not supported"),
        "{text}"
    );
    assert!(!text.contains("hunter2") && !text.contains("amy"), "{text}");
    for garbage in [
        "not a url",
        "ftp://proxy.example:21",
        "http://",
        "socks5://[::1]:1080",
    ] {
        let output = fx.run(&["config", "set", "network.proxy", garbage]).await;
        assert_exit(&output, 1);
        assert!(
            stderr(&output).contains("unsupported proxy"),
            "{garbage}: {}",
            stderr(&output)
        );
    }
    assert_eq!(
        proxy_line(&fx).await,
        "network.proxy = http://proxy.example:3128\n"
    );
}
