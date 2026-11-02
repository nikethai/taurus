use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use super::*;
use crate::sha256_hex;

fn run(cmd: &mut Command) -> Output {
    run_retrying(cmd).expect("run fake tool")
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn scenario_file(dir: &Path, name: &str, json: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, json).unwrap();
    path
}

#[test]
fn fake_tool_reports_version() {
    let dir = tempfile::tempdir().unwrap();
    let yt = write_fake_tool(dir.path(), "yt-dlp", "2026.01.01", false);
    assert!(yt.ends_with(format!("yt-dlp{}", std::env::consts::EXE_SUFFIX)));
    let out = run(Command::new(&yt).arg("--version"));
    assert!(out.status.success(), "{out:?}");
    assert_eq!(stdout(&out).trim(), "2026.01.01");

    let ffmpeg = write_fake_tool(dir.path(), "ffmpeg", "N-12345", false);
    let out = run(Command::new(&ffmpeg).arg("-version"));
    assert!(out.status.success());
    assert!(stdout(&out).starts_with("ffmpeg version N-12345 "));

    let deno = write_fake_tool(dir.path(), "deno", "2.9.8", false);
    let out = run(Command::new(&deno).arg("--version"));
    assert!(stdout(&out).starts_with("deno 2.9.8 ("), "{out:?}");
}

#[test]
fn fake_tool_without_trailer_reads_env() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir
        .path()
        .join(format!("ffprobe{}", std::env::consts::EXE_SUFFIX));
    write_executable(&path, &fake_tool_bytes());
    let out = run(Command::new(&path).arg("-version"));
    assert!(stdout(&out).starts_with("ffprobe version 0.0.0 "));
    let out = run(Command::new(&path)
        .arg("-version")
        .env("FAKE_TOOL_VERSION", "7.1"));
    assert!(stdout(&out).starts_with("ffprobe version 7.1 "));
    let out = run(Command::new(&path)
        .arg("-version")
        .env("FAKE_TOOL_BROKEN", "1"));
    assert_eq!(out.status.code(), Some(3));
}

#[test]
fn fake_tool_broken_exits_3() {
    let dir = tempfile::tempdir().unwrap();
    let yt = write_fake_tool(dir.path(), "yt-dlp", "2026.01.01", true);
    let out = run(Command::new(&yt).arg("--version"));
    assert_eq!(out.status.code(), Some(3));
    assert!(out.stdout.is_empty());
}

#[test]
fn fake_tool_hang_waits_until_killed() {
    let dir = tempfile::tempdir().unwrap();
    let version = format!("2026.7.{}", fastrand_like());
    let mut binary = fake_tool_bytes();
    package::append_trailer(&mut binary, &version, false, true);
    let yt = dir
        .path()
        .join(format!("yt-dlp{}", std::env::consts::EXE_SUFFIX));
    write_executable(&yt, &binary);
    let pid_file = hang_pid_file(&version);
    let _ = std::fs::remove_file(&pid_file);
    let mut child = loop {
        match Command::new(&yt).arg("--version").spawn() {
            Err(e) if e.raw_os_error() == Some(26) => {
                std::thread::sleep(std::time::Duration::from_millis(20))
            }
            other => break other.unwrap(),
        }
    };
    let started = std::time::Instant::now();
    while !pid_file.exists() {
        assert!(started.elapsed().as_secs() < 10, "no pid file");
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    std::thread::sleep(std::time::Duration::from_millis(200));
    assert!(child.try_wait().unwrap().is_none(), "still running");
    let pid = std::fs::read_to_string(&pid_file).unwrap();
    assert_eq!(pid, child.id().to_string());
    child.kill().unwrap();
    child.wait().unwrap();
    let _ = std::fs::remove_file(&pid_file);
}

/// A number unlikely to repeat across concurrent test runs.
fn fastrand_like() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos()
}

#[test]
fn fake_ytdlp_replays_scenario() {
    let dir = tempfile::tempdir().unwrap();
    let yt = write_fake_tool(dir.path(), "yt-dlp", "2026.01.01", false);
    let abs = dir.path().join("abs").join("b.bin");
    let scenario = format!(
        r#"{{ "stdout": ["first line", "{{\"progress\": \"50%\"}}", "café"],
             "stderr": ["ERROR: something"],
             "exit": 4,
             "start_files": [["early.part", 5]],
             "write_files": [["sub/a.mp4", 10], [{abs}, 3]] }}"#,
        abs = json_quote(&abs.to_string_lossy())
    );
    let scenario = scenario_file(dir.path(), "s.json", &scenario);
    let log = dir.path().join("args.log");
    let work = dir.path().join("work");
    std::fs::create_dir(&work).unwrap();
    let out = run(Command::new(&yt)
        .args(["-o", "a \"b\" \\c", "https://x"])
        .current_dir(&work)
        .env("FAKE_YTDLP_SCENARIO", &scenario)
        .env("FAKE_TOOL_ARGS_LOG", &log));
    assert_eq!(out.status.code(), Some(4));
    assert_eq!(stdout(&out), "first line\n{\"progress\": \"50%\"}\ncafé\n");
    assert_eq!(String::from_utf8_lossy(&out.stderr), "ERROR: something\n");
    assert_eq!(std::fs::metadata(work.join("sub/a.mp4")).unwrap().len(), 10);
    assert_eq!(std::fs::metadata(&abs).unwrap().len(), 3);
    assert_eq!(std::fs::metadata(work.join("early.part")).unwrap().len(), 5);

    // A second run appends a second line.
    run(Command::new(&yt)
        .arg("--version")
        .env("FAKE_TOOL_ARGS_LOG", &log));
    let entries = read_args_log(&log);
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].tool, "yt-dlp");
    assert_eq!(entries[0].argv, ["-o", "a \"b\" \\c", "https://x"]);
    assert!(entries[0].pid > 0);
    assert_eq!(entries[1].argv, ["--version"]);
}

#[test]
fn fake_ytdlp_picks_entry_by_arg_and_version() {
    let dir = tempfile::tempdir().unwrap();
    scenario_file(
        dir.path(),
        "s-2026.01.01.json",
        r#"[ { "if_arg": "-J", "stdout": ["{\"title\": \"x\"}"] },
             { "stdout": ["downloading"], "exit": 0 } ]"#,
    );
    scenario_file(
        dir.path(),
        "s-2026.02.01.json",
        r#"{ "stderr": ["ERROR: [youtube] x: Signature extraction failed"], "exit": 1 }"#,
    );
    let pattern = dir.path().join("s-{version}.json");
    let old = write_fake_tool(dir.path(), "yt-dlp", "2026.01.01", false);
    let out = run(Command::new(&old)
        .args(["--flat-playlist", "-J", "u"])
        .env("FAKE_YTDLP_SCENARIO", &pattern));
    assert_eq!(stdout(&out).trim(), "{\"title\": \"x\"}");
    let out = run(Command::new(&old)
        .arg("u")
        .env("FAKE_YTDLP_SCENARIO", &pattern));
    assert_eq!(stdout(&out).trim(), "downloading");

    let new_dir = dir.path().join("new");
    std::fs::create_dir(&new_dir).unwrap();
    let new = write_fake_tool(&new_dir, "yt-dlp", "2026.02.01", false);
    let out = run(Command::new(&new)
        .arg("u")
        .env("FAKE_YTDLP_SCENARIO", &pattern));
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("Signature extraction failed"));

    // Typos in a scenario are reported, not ignored.
    let bad = scenario_file(dir.path(), "bad.json", r#"{ "exit_code": 1 }"#);
    let out = run(Command::new(&old).arg("u").env("FAKE_YTDLP_SCENARIO", &bad));
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("exit_code"));
}

#[test]
fn fake_ffmpeg_copy_mode() {
    let dir = tempfile::tempdir().unwrap();
    let ffmpeg = write_fake_tool(dir.path(), "ffmpeg", "N-1", false);
    let input = dir.path().join("live.mp4.part");
    let output = dir.path().join("live.mp4");
    std::fs::write(&input, b"stream bytes").unwrap();
    let out = run(Command::new(&ffmpeg)
        .arg("-y")
        .arg("-i")
        .arg(&input)
        .args(["-c", "copy"])
        .arg(&output));
    assert!(out.status.success(), "{out:?}");
    assert_eq!(std::fs::read(&output).unwrap(), b"stream bytes");

    // Several inputs (a video and an audio part): joined in order.
    let audio = dir.path().join("live.m4a.part");
    std::fs::write(&audio, b" + audio").unwrap();
    let joined = dir.path().join("joined.mp4");
    let out = run(Command::new(&ffmpeg)
        .arg("-y")
        .arg("-i")
        .arg(&input)
        .arg("-i")
        .arg(&audio)
        .args(["-map", "0", "-map", "1", "-c", "copy"])
        .arg(&joined));
    assert!(out.status.success(), "{out:?}");
    assert_eq!(std::fs::read(&joined).unwrap(), b"stream bytes + audio");
}

#[cfg(unix)]
#[test]
fn fake_ytdlp_on_interrupt() {
    use std::io::{BufRead, BufReader};
    use std::process::Stdio;
    use std::thread;
    use std::time::Duration;

    unsafe extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    let dir = tempfile::tempdir().unwrap();
    let yt = write_fake_tool(dir.path(), "yt-dlp", "2026.01.01", false);
    let scenario = scenario_file(
        dir.path(),
        "s.json",
        r#"{ "stdout": ["one", "two", "three", "four"], "exit": 0,
             "write_files": [["x.mp4", 100]],
             "delay_ms_per_line": 200, "wait_for_interrupt": true,
             "on_interrupt": { "stdout": ["interrupted"], "write_files": [["x.mp4.part", 7]], "exit": 1 } }"#,
    );
    let mut child = loop {
        let spawned = Command::new(&yt)
            .arg("u")
            .current_dir(dir.path())
            .env("FAKE_YTDLP_SCENARIO", &scenario)
            .stdout(Stdio::piped())
            .spawn();
        match spawned {
            Err(e) if e.raw_os_error() == Some(26) => thread::sleep(Duration::from_millis(20)),
            other => break other.unwrap(),
        }
    };
    let mut reader = BufReader::new(child.stdout.take().unwrap());
    let mut first = String::new();
    reader.read_line(&mut first).unwrap();
    assert_eq!(first, "one\n");
    // SAFETY: plain libc call on our own child's pid; SIGINT = 2.
    assert_eq!(unsafe { kill(child.id() as i32, 2) }, 0);
    let mut rest = String::new();
    reader.read_to_string(&mut rest).unwrap();
    let status = child.wait().unwrap();
    assert_eq!(status.code(), Some(1));
    assert_eq!(rest, "interrupted\n");
    assert_eq!(
        std::fs::metadata(dir.path().join("x.mp4.part"))
            .unwrap()
            .len(),
        7
    );
    assert!(!dir.path().join("x.mp4").exists());
}

fn json_quote(s: &str) -> String {
    serde_json::to_string(s).unwrap()
}

// ---------------------------------------------------------------- host

fn release(version: &str) -> ToolRelease {
    ToolRelease::new(version)
}

async fn get(url: &str) -> reqwest::Response {
    let resp = reqwest::get(url).await.expect("request");
    assert!(resp.status().is_success(), "{url}: {}", resp.status());
    resp
}

async fn text(url: &str) -> String {
    get(url).await.text().await.unwrap()
}

async fn bytes(url: &str) -> Vec<u8> {
    get(url).await.bytes().await.unwrap().to_vec()
}

/// The hash a sha256sum-format file lists for `name`.
fn listed(sums: &str, name: &str) -> String {
    sums.lines()
        .find_map(|l| l.strip_suffix(&format!("  {name}")))
        .unwrap_or_else(|| panic!("{name} missing from {sums}"))
        .to_owned()
}

fn zip_names(bytes: &[u8]) -> Vec<String> {
    let archive = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
    archive
        .file_names()
        .map(|n| n.unwrap().into_owned())
        .collect()
}

fn zip_member(bytes: &[u8], name: &str) -> Vec<u8> {
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
    let mut out = Vec::new();
    archive
        .by_name(name)
        .unwrap()
        .read_to_end(&mut out)
        .unwrap();
    out
}

/// (path, content) of every regular file in a .tar.xz.
fn tar_xz_files(bytes: &[u8]) -> Vec<(String, Vec<u8>)> {
    let xz = lzma_rust2::XzReader::new(std::io::Cursor::new(bytes), false);
    let mut archive = tar::Archive::new(xz);
    let mut files = Vec::new();
    for entry in archive.entries().unwrap() {
        let mut entry = entry.unwrap();
        if entry.header().entry_type().is_file() {
            let path = entry.path().unwrap().to_string_lossy().into_owned();
            let mut content = Vec::new();
            entry.read_to_end(&mut content).unwrap();
            files.push((path, content));
        }
    }
    files
}

fn native_ytdlp_asset() -> &'static str {
    if cfg!(windows) {
        if cfg!(target_arch = "aarch64") {
            "yt-dlp_arm64.exe"
        } else {
            "yt-dlp.exe"
        }
    } else if cfg!(target_os = "macos") {
        "yt-dlp_macos"
    } else if cfg!(target_arch = "aarch64") {
        "yt-dlp_linux_aarch64"
    } else {
        "yt-dlp_linux"
    }
}

/// Writes `bytes` as `<dir>/<tool>[.exe]` and returns `<tool> <version arg>`'s stdout.
fn version_of(dir: &Path, tool: &str, bytes: &[u8]) -> Output {
    let path = dir.join(format!("{tool}{}", std::env::consts::EXE_SUFFIX));
    write_executable(&path, bytes);
    let arg = if tool.starts_with("ff") {
        "-version"
    } else {
        "--version"
    };
    run(Command::new(path).arg(arg))
}

#[tokio::test]
async fn host_serves_publisher_layouts() {
    let host = FakeToolHost::start(release("2026.01.01"), release("N-1"), release("2.9.8")).await;
    let base = host.base_url();
    assert!(base.starts_with("http://127.0.0.1:") && base.ends_with('/'));
    let dir = tempfile::tempdir().unwrap();

    // yt-dlp: raw binaries listed in SHA2-256SUMS.
    let sums = text(&format!("{base}yt-dlp/SHA2-256SUMS")).await;
    assert_eq!(sums.lines().count(), 7);
    for asset in package::YT_DLP_ASSETS {
        listed(&sums, asset);
    }
    let asset = native_ytdlp_asset();
    let binary = bytes(&format!("{base}yt-dlp/{asset}")).await;
    assert_eq!(sha256_hex(&binary), listed(&sums, asset));
    let out = version_of(dir.path(), "yt-dlp", &binary);
    assert_eq!(stdout(&out).trim(), "2026.01.01");

    // ffmpeg Linux: tar.xz with <root>/bin/ffmpeg and bin/ffprobe.
    let sums = text(&format!("{base}ffmpeg-builds/checksums.sha256")).await;
    assert_eq!(sums.lines().count(), 4);
    let name = "ffmpeg-master-latest-linux64-gpl.tar.xz";
    let archive = bytes(&format!("{base}ffmpeg-builds/{name}")).await;
    assert_eq!(sha256_hex(&archive), listed(&sums, name));
    let files = tar_xz_files(&archive);
    let paths: Vec<&str> = files.iter().map(|(p, _)| p.as_str()).collect();
    assert!(
        paths.contains(&"ffmpeg-master-latest-linux64-gpl/bin/ffmpeg"),
        "{paths:?}"
    );
    assert!(paths.contains(&"ffmpeg-master-latest-linux64-gpl/bin/ffprobe"));
    let ffmpeg = &files
        .iter()
        .find(|(p, _)| p.ends_with("bin/ffmpeg"))
        .unwrap()
        .1;
    assert!(stdout(&version_of(dir.path(), "ffmpeg", ffmpeg)).starts_with("ffmpeg version N-1 "));

    // ffmpeg Windows: zip with bin/ffmpeg.exe and bin/ffprobe.exe.
    let name = "ffmpeg-master-latest-winarm64-gpl.zip";
    let archive = bytes(&format!("{base}ffmpeg-builds/{name}")).await;
    assert_eq!(sha256_hex(&archive), listed(&sums, name));
    let names = zip_names(&archive);
    assert!(names.contains(&"ffmpeg-master-latest-winarm64-gpl/bin/ffmpeg.exe".to_owned()));
    assert!(names.contains(&"ffmpeg-master-latest-winarm64-gpl/bin/ffprobe.exe".to_owned()));

    // Deno: zip + sidecar; Windows sidecars are PowerShell Format-List output.
    let name = "deno-x86_64-unknown-linux-gnu.zip";
    let archive = bytes(&format!("{base}deno/{name}")).await;
    let sidecar = text(&format!("{base}deno/{name}.sha256sum")).await;
    assert_eq!(sidecar, format!("{}  {name}\n", sha256_hex(&archive)));
    assert_eq!(zip_names(&archive), ["deno"]);
    let deno = zip_member(&archive, "deno");
    assert!(stdout(&version_of(dir.path(), "deno", &deno)).starts_with("deno 2.9.8 "));
    let name = "deno-aarch64-pc-windows-msvc.zip";
    let archive = bytes(&format!("{base}deno/{name}")).await;
    assert_eq!(zip_names(&archive), ["deno.exe"]);
    let sidecar = text(&format!("{base}deno/{name}.sha256sum")).await;
    let hash = sha256_hex(&archive).to_uppercase();
    assert!(
        sidecar.contains(&format!("\r\nHash      : {hash}\r\n")),
        "{sidecar:?}"
    );

    // ffmpeg macOS: a redirect to the resolved zip, `.sha256` beside it.
    let resp = get(&format!("{base}ffmpeg-macos/arm64/release/ffprobe.zip")).await;
    let resolved = resp.url().to_string();
    assert!(
        resolved.starts_with(&format!("{base}ffmpeg-macos-files/N-1/arm64/")),
        "{resolved}"
    );
    assert!(resolved.ends_with("/ffprobe.zip"));
    let archive = resp.bytes().await.unwrap().to_vec();
    let sidecar = text(&format!("{resolved}.sha256")).await;
    assert_eq!(sidecar, format!("{}  ffprobe.zip\n", sha256_hex(&archive)));
    assert_eq!(zip_names(&archive), ["ffprobe"]);

    // Release APIs.
    let api = format!("{base}api/repos");
    let yt: serde_json::Value = get(&format!("{api}/yt-dlp/yt-dlp/releases/latest"))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(yt["tag_name"], "2026.01.01");
    let deno: serde_json::Value = get(&format!("{api}/denoland/deno/releases/latest"))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(deno["tag_name"], "v2.9.8");

    // Unknown paths are 404; every request is counted.
    let missing = reqwest::get(format!("{base}yt-dlp/yt-dlp_x86.exe"))
        .await
        .unwrap();
    assert_eq!(missing.status(), 404);
    assert_eq!(host.requests("yt-dlp/"), 3);
    assert_eq!(host.requests("ffmpeg-macos/"), 1);
    assert_eq!(host.requests("ffmpeg-macos-files/"), 2);
    assert_eq!(host.requests("api/"), 2);
    assert_eq!(host.requests(""), 15);
}

#[tokio::test]
async fn host_set_release_changes_what_is_served() {
    let host = FakeToolHost::start(release("2026.01.01"), release("N-1"), release("2.9.8")).await;
    let base = host.base_url();
    let dir = tempfile::tempdir().unwrap();
    let asset = native_ytdlp_asset();

    host.set_release(
        "yt-dlp",
        ToolRelease {
            corrupt_checksum: true,
            ..release("2026.02.01")
        },
    );
    let api: serde_json::Value = get(&format!("{base}api/repos/yt-dlp/yt-dlp/releases/latest"))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(api["tag_name"], "2026.02.01");
    let binary = bytes(&format!("{base}yt-dlp/{asset}")).await;
    let sums = text(&format!("{base}yt-dlp/SHA2-256SUMS")).await;
    let hash = listed(&sums, asset);
    assert_eq!(hash.len(), 64);
    assert_ne!(sha256_hex(&binary), hash);
    assert_eq!(
        stdout(&version_of(dir.path(), "yt-dlp", &binary)).trim(),
        "2026.02.01"
    );

    host.set_release(
        "ffmpeg",
        ToolRelease {
            broken: true,
            ..release("N-2")
        },
    );
    let name = "ffmpeg-master-latest-linuxarm64-gpl.tar.xz";
    let archive = bytes(&format!("{base}ffmpeg-builds/{name}")).await;
    let sums = text(&format!("{base}ffmpeg-builds/checksums.sha256")).await;
    assert_eq!(sha256_hex(&archive), listed(&sums, name));
    let files = tar_xz_files(&archive);
    let ffprobe = &files
        .iter()
        .find(|(p, _)| p.ends_with("bin/ffprobe"))
        .unwrap()
        .1;
    assert_eq!(
        version_of(dir.path(), "ffprobe", ffprobe).status.code(),
        Some(3)
    );
    let resp = get(&format!("{base}ffmpeg-macos/amd64/release/ffmpeg.zip")).await;
    assert!(
        resp.url()
            .path()
            .starts_with("/ffmpeg-macos-files/N-2/amd64/")
    );

    host.set_release(
        "deno",
        ToolRelease {
            corrupt_checksum: true,
            ..release("2.10.0")
        },
    );
    let name = "deno-aarch64-apple-darwin.zip";
    let archive = bytes(&format!("{base}deno/{name}")).await;
    let sidecar = text(&format!("{base}deno/{name}.sha256sum")).await;
    assert_ne!(sidecar, format!("{}  {name}\n", sha256_hex(&archive)));
    assert!(sidecar.ends_with(&format!("  {name}\n")));
}

#[tokio::test]
async fn host_goes_offline_and_back_on_the_same_port() {
    let host = FakeToolHost::start(release("2026.01.01"), release("N-1"), release("2.9.8")).await;
    let base = host.base_url();
    let url = format!("{base}api/repos/denoland/deno/releases/latest");
    let client = reqwest::Client::new();
    assert!(client.get(&url).send().await.unwrap().status().is_success());

    host.go_offline();
    let err = client.get(&url).send().await.unwrap_err();
    assert!(err.is_connect(), "{err:?}");
    assert_eq!(host.base_url(), base);

    host.go_online();
    assert_eq!(host.base_url(), base);
    assert!(client.get(&url).send().await.unwrap().status().is_success());
    assert_eq!(host.requests("api/"), 2);
}

#[test]
fn shared_scenarios_are_keyed_by_version() {
    let (a, b) = (unique_version(), unique_version());
    assert_ne!(a, b);
    set_ytdlp_scenario(&a, r#"{"stdout": ["from a"]}"#);
    set_ytdlp_scenario(&b, r#"{"stdout": ["from b"], "exit": 5}"#);
    let dir = tempfile::tempdir().unwrap();
    for (version, line, code) in [(&a, "from a", 0), (&b, "from b", 5)] {
        let yt = write_fake_tool(dir.path(), "yt-dlp", version, false);
        let out = run(Command::new(&yt).arg(format!("--marker-{version}")));
        assert_eq!(stdout(&out).trim(), line);
        assert_eq!(out.status.code(), Some(code));
        let logged: Vec<_> = process_args_log()
            .into_iter()
            .filter(|e| e.argv == [format!("--marker-{version}")])
            .collect();
        assert_eq!(logged.len(), 1);
        assert_eq!(logged[0].tool, "yt-dlp");
    }
}
