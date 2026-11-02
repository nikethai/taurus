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
        cmd.env_remove("TAURUS_DATA_DIR")
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
