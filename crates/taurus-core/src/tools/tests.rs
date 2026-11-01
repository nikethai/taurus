//! ToolManager against the testkit's fake publishers.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

use taurus_testkit::{FakeToolHost, ToolRelease, hang_pid_file, run_retrying};

use super::*;
use crate::errors::ErrorKind;

const YT1: &str = "2026.01.01";
const YT2: &str = "2026.02.01";

struct Fixture {
    tmp: tempfile::TempDir,
    host: FakeToolHost,
    manager: ToolManager,
}

impl Fixture {
    async fn new() -> Fixture {
        let host = FakeToolHost::start(
            ToolRelease::new(YT1),
            ToolRelease::new("7.1"),
            ToolRelease::new("2.5.0"),
        )
        .await;
        let tmp = tempfile::tempdir().unwrap();
        let manager = manager_for(tmp.path(), &host);
        Fixture { tmp, host, manager }
    }

    fn tool_dir(&self, kind: ToolKind) -> PathBuf {
        self.tmp.path().join("tools").join(kind.name())
    }

    fn current(&self, kind: ToolKind) -> String {
        std::fs::read_to_string(self.tool_dir(kind).join("current"))
            .unwrap()
            .trim()
            .to_owned()
    }

    fn status_of(&self, kind: ToolKind) -> ToolStatus {
        self.manager
            .status()
            .into_iter()
            .find(|s| s.kind == kind)
            .unwrap()
    }

    async fn ensure(&self, kind: ToolKind) -> PathBuf {
        self.manager.ensure(kind, &|_| {}).await.unwrap()
    }

    async fn update(&self, kind: ToolKind, force: bool) -> Result<bool, CoreError> {
        self.manager.update(kind, force, &|_| {}).await
    }
}

fn manager_for(data_dir: &Path, host: &FakeToolHost) -> ToolManager {
    let base = url::Url::parse(&host.base_url()).unwrap();
    ToolManager::new(
        data_dir,
        ToolSources::under(&base),
        crate::http::build_client(),
    )
}

fn version_output(path: &Path, arg: &str) -> String {
    let out = run_retrying(Command::new(path).arg(arg)).unwrap();
    assert!(out.status.success(), "{} {arg} failed", path.display());
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn exe(name: &str) -> String {
    format!("{name}{}", std::env::consts::EXE_SUFFIX)
}

#[tokio::test]
async fn ensure_installs_all_three() {
    let f = Fixture::new().await;
    for kind in ToolKind::ALL {
        assert_eq!(f.status_of(kind).state, ToolState::Missing);
    }
    let seen = Mutex::new(Vec::new());
    let record = |s: ToolStatus| seen.lock().unwrap().push(s);
    let yt = f.manager.ensure(ToolKind::YtDlp, &record).await.unwrap();
    let ffmpeg = f.manager.ensure(ToolKind::Ffmpeg, &record).await.unwrap();
    let deno = f.manager.ensure(ToolKind::Deno, &record).await.unwrap();

    assert_eq!(yt.file_name().unwrap().to_string_lossy(), exe("yt-dlp"));
    assert_eq!(version_output(&yt, "--version").trim(), YT1);
    assert!(version_output(&ffmpeg, "-version").contains("ffmpeg version 7.1"));
    let ffprobe = ffmpeg.with_file_name(exe("ffprobe"));
    assert!(version_output(&ffprobe, "-version").contains("ffprobe version 7.1"));
    assert!(version_output(&deno, "--version").starts_with("deno 2.5.0"));

    assert_eq!(f.current(ToolKind::YtDlp), YT1);
    assert_eq!(f.current(ToolKind::Deno), "2.5.0");
    let ffmpeg_version = f.current(ToolKind::Ffmpeg);
    assert_eq!(ffmpeg_version.len(), 12, "{ffmpeg_version}");
    assert!(ffmpeg_version.chars().all(|c| c.is_ascii_hexdigit()));
    assert_eq!(
        ffmpeg,
        f.tool_dir(ToolKind::Ffmpeg)
            .join(&ffmpeg_version)
            .join(exe("ffmpeg"))
    );

    for (kind, path) in [
        (ToolKind::YtDlp, &yt),
        (ToolKind::Ffmpeg, &ffmpeg),
        (ToolKind::Deno, &deno),
    ] {
        let status = f.status_of(kind);
        assert_eq!(status.state, ToolState::Ready, "{kind:?}");
        assert_eq!(status.path.as_ref(), Some(path));
        assert_eq!(status.version.as_deref(), Some(f.current(kind).as_str()));
    }
    let seen = seen.into_inner().unwrap();
    for kind in ToolKind::ALL {
        let of_kind: Vec<_> = seen.iter().filter(|s| s.kind == kind).collect();
        assert!(
            matches!(of_kind.first().unwrap().state, ToolState::Installing { .. }),
            "{of_kind:?}"
        );
        assert_eq!(of_kind.last().unwrap().state, ToolState::Ready);
    }
    // Nothing is left behind in the staging folder.
    assert!(!f.tool_dir(ToolKind::YtDlp).join(".incoming").exists());

    // Installed: ensure makes no request.
    let before = f.host.requests("");
    assert_eq!(f.ensure(ToolKind::YtDlp).await, yt);
    assert_eq!(f.host.requests(""), before);
}

#[tokio::test]
async fn update_switches_and_keeps_previous() {
    let f = Fixture::new().await;
    let old = f.ensure(ToolKind::YtDlp).await;
    // Same version published: nothing to do.
    assert!(!f.update(ToolKind::YtDlp, true).await.unwrap());

    f.host.set_release("yt-dlp", ToolRelease::new(YT2));
    assert!(f.update(ToolKind::YtDlp, true).await.unwrap());
    assert_eq!(f.current(ToolKind::YtDlp), YT2);
    let new = f.ensure(ToolKind::YtDlp).await;
    assert_eq!(version_output(&new, "--version").trim(), YT2);
    assert!(old.exists(), "previous version is kept");
    assert_eq!(f.status_of(ToolKind::YtDlp).version.as_deref(), Some(YT2));

    // A third version removes the first.
    f.host.set_release("yt-dlp", ToolRelease::new("2026.03.01"));
    assert!(f.update(ToolKind::YtDlp, true).await.unwrap());
    assert!(!f.tool_dir(ToolKind::YtDlp).join(YT1).exists());
    assert!(new.exists());

    // ffmpeg's version follows its archive.
    f.ensure(ToolKind::Ffmpeg).await;
    let first = f.current(ToolKind::Ffmpeg);
    assert!(!f.update(ToolKind::Ffmpeg, true).await.unwrap());
    f.host.set_release("ffmpeg", ToolRelease::new("7.2"));
    assert!(f.update(ToolKind::Ffmpeg, true).await.unwrap());
    assert_ne!(f.current(ToolKind::Ffmpeg), first);
    assert!(f.tool_dir(ToolKind::Ffmpeg).join(&first).exists());
}

#[tokio::test]
async fn bad_checksum_keeps_previous() {
    let f = Fixture::new().await;
    let old = f.ensure(ToolKind::YtDlp).await;
    f.host.set_release(
        "yt-dlp",
        ToolRelease {
            corrupt_checksum: true,
            ..ToolRelease::new(YT2)
        },
    );
    let err = f.update(ToolKind::YtDlp, true).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::ChecksumMismatch, "{err}");
    assert_eq!(f.current(ToolKind::YtDlp), YT1);
    assert!(!f.tool_dir(ToolKind::YtDlp).join(YT2).exists());
    assert!(!f.tool_dir(ToolKind::YtDlp).join(".incoming").exists());
    assert_eq!(version_output(&old, "--version").trim(), YT1);
    assert_eq!(f.status_of(ToolKind::YtDlp).state, ToolState::Ready);

    // A first install with a bad checksum installs nothing.
    f.host.set_release(
        "deno",
        ToolRelease {
            corrupt_checksum: true,
            ..ToolRelease::new("2.5.0")
        },
    );
    let err = f.manager.ensure(ToolKind::Deno, &|_| {}).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::ChecksumMismatch);
    assert_eq!(
        f.status_of(ToolKind::Deno).state,
        ToolState::Failed(ErrorKind::ChecksumMismatch)
    );
    assert!(!f.tool_dir(ToolKind::Deno).join("current").exists());
}

#[tokio::test]
async fn broken_binary_rolls_back() {
    let f = Fixture::new().await;
    f.ensure(ToolKind::YtDlp).await;
    f.host.set_release(
        "yt-dlp",
        ToolRelease {
            broken: true,
            ..ToolRelease::new(YT2)
        },
    );
    let last = Mutex::new(None);
    let err = f
        .manager
        .update(ToolKind::YtDlp, true, &|s| *last.lock().unwrap() = Some(s))
        .await
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::ToolFailed, "{err}");
    assert_eq!(f.current(ToolKind::YtDlp), YT1);
    assert!(!f.tool_dir(ToolKind::YtDlp).join(YT2).exists());
    assert!(!f.tool_dir(ToolKind::YtDlp).join(".incoming").exists());
    let last = last.into_inner().unwrap().unwrap();
    assert_eq!(last.state, ToolState::Failed(ErrorKind::ToolFailed));
    assert_eq!(last.version.as_deref(), Some(YT1));
    assert_eq!(f.status_of(ToolKind::YtDlp).state, ToolState::Ready);

    // A broken ffprobe fails ffmpeg too.
    f.host.set_release(
        "ffmpeg",
        ToolRelease {
            broken: true,
            ..ToolRelease::new("7.1")
        },
    );
    let err = f
        .manager
        .ensure(ToolKind::Ffmpeg, &|_| {})
        .await
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::ToolFailed);
    assert!(!f.tool_dir(ToolKind::Ffmpeg).join("current").exists());
}

#[tokio::test]
async fn rollback_restores_previous() {
    let f = Fixture::new().await;
    let err = f.manager.rollback(ToolKind::YtDlp).unwrap_err();
    assert_eq!(err.kind, ErrorKind::ToolMissing);

    let old = f.ensure(ToolKind::YtDlp).await;
    f.host.set_release("yt-dlp", ToolRelease::new(YT2));
    assert!(f.update(ToolKind::YtDlp, true).await.unwrap());
    f.manager.rollback(ToolKind::YtDlp).unwrap();
    assert_eq!(f.current(ToolKind::YtDlp), YT1);
    let status = f.status_of(ToolKind::YtDlp);
    assert_eq!(status.version.as_deref(), Some(YT1));
    assert_eq!(status.path.as_ref(), Some(&old));
    assert_eq!(version_output(&old, "--version").trim(), YT1);
    // The version rolled back from stays, so rolling back again returns to it.
    assert!(f.tool_dir(ToolKind::YtDlp).join(YT2).exists());
    f.manager.rollback(ToolKind::YtDlp).unwrap();
    assert_eq!(f.current(ToolKind::YtDlp), YT2);
}

#[tokio::test]
async fn rollback_is_sticky_until_a_newer_release() {
    let f = Fixture::new().await;
    f.ensure(ToolKind::YtDlp).await;
    f.host.set_release("yt-dlp", ToolRelease::new(YT2));
    assert!(f.update(ToolKind::YtDlp, true).await.unwrap());
    f.manager.rollback(ToolKind::YtDlp).unwrap();
    // The publisher still has the version rolled back from: kept away,
    // even when forced, and its kept folder is left as it is.
    let downloads = f.host.requests("yt-dlp/");
    assert!(!f.update(ToolKind::YtDlp, true).await.unwrap());
    assert_eq!(f.current(ToolKind::YtDlp), YT1);
    assert_eq!(f.host.requests("yt-dlp/"), downloads, "nothing downloaded");
    assert!(f.tool_dir(ToolKind::YtDlp).join(YT2).exists());
    // A newer release installs, and ends the skip.
    f.host.set_release("yt-dlp", ToolRelease::new("2026.03.01"));
    assert!(f.update(ToolKind::YtDlp, true).await.unwrap());
    assert_eq!(f.current(ToolKind::YtDlp), "2026.03.01");
    assert!(
        !f.tool_dir(ToolKind::YtDlp)
            .join("rolled-back-from")
            .exists()
    );

    // Likewise for ffmpeg, whose version is its archive's hash.
    f.ensure(ToolKind::Ffmpeg).await;
    let first = f.current(ToolKind::Ffmpeg);
    f.host.set_release("ffmpeg", ToolRelease::new("7.2"));
    assert!(f.update(ToolKind::Ffmpeg, true).await.unwrap());
    f.manager.rollback(ToolKind::Ffmpeg).unwrap();
    assert!(!f.update(ToolKind::Ffmpeg, true).await.unwrap());
    assert_eq!(f.current(ToolKind::Ffmpeg), first);
}

#[tokio::test]
async fn reinstall_keeps_a_working_previous_version() {
    let f = Fixture::new().await;
    let old = f.ensure(ToolKind::YtDlp).await;
    f.host.set_release("yt-dlp", ToolRelease::new(YT2));
    assert!(f.update(ToolKind::YtDlp, true).await.unwrap());
    // The active binary disappears: ensure installs again, keeping YT1.
    let new = f.ensure(ToolKind::YtDlp).await;
    std::fs::remove_file(&new).unwrap();
    assert_eq!(f.status_of(ToolKind::YtDlp).state, ToolState::Missing);
    assert_eq!(f.ensure(ToolKind::YtDlp).await, new);
    assert!(old.exists(), "the working previous version is kept");
    f.manager.rollback(ToolKind::YtDlp).unwrap();
    assert_eq!(f.current(ToolKind::YtDlp), YT1);

    // An unreadable `current` keeps the working folders too.
    std::fs::write(f.tool_dir(ToolKind::YtDlp).join("current"), "../..").unwrap();
    f.ensure(ToolKind::YtDlp).await;
    assert_eq!(f.current(ToolKind::YtDlp), YT2);
    assert!(old.exists());
}

#[tokio::test]
async fn a_hanging_binary_times_out_and_is_killed() {
    let f = Fixture::new().await;
    let timeout = std::time::Duration::from_millis(500);
    let manager = manager_for(f.tmp.path(), &f.host).with_version_timeout(timeout);
    manager.ensure(ToolKind::YtDlp, &|_| {}).await.unwrap();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .subsec_nanos();
    let hanging = format!("2026.9.{nanos}");
    let pid_file = hang_pid_file(&hanging);
    let _ = std::fs::remove_file(&pid_file);
    f.host.set_release(
        "yt-dlp",
        ToolRelease {
            hang: true,
            ..ToolRelease::new(&hanging)
        },
    );
    let started = std::time::Instant::now();
    let err = manager
        .update(ToolKind::YtDlp, true, &|_| {})
        .await
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::ToolFailed, "{err}");
    assert!(err.detail.contains("did not answer"), "{err}");
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
    assert_eq!(f.current(ToolKind::YtDlp), YT1);
    assert!(!f.tool_dir(ToolKind::YtDlp).join(&hanging).exists());
    let pid: i32 = std::fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let _ = std::fs::remove_file(&pid_file);
    // Killed and reaped before `update` returned.
    #[cfg(unix)]
    {
        // SAFETY: signal 0 only checks whether the process exists.
        let alive = unsafe { libc::kill(pid, 0) } == 0;
        assert!(!alive, "pid {pid} still exists");
    }
    #[cfg(not(unix))]
    let _ = pid;
}

#[tokio::test]
async fn update_respects_daily_rule() {
    let f = Fixture::new().await;
    // Not installed: an update installs nothing.
    assert!(!f.update(ToolKind::YtDlp, false).await.unwrap());
    assert!(!f.tool_dir(ToolKind::YtDlp).join("current").exists());

    f.ensure(ToolKind::YtDlp).await;
    f.host.set_release("yt-dlp", ToolRelease::new(YT2));
    let before = f.host.requests("");
    // Installed moments ago: checked.
    assert!(!f.update(ToolKind::YtDlp, false).await.unwrap());
    assert!(!f.update(ToolKind::YtDlp, false).await.unwrap());
    assert_eq!(f.host.requests(""), before, "no request within 24 h");
    assert_eq!(f.current(ToolKind::YtDlp), YT1);
    // Forced, it checks at once.
    assert!(f.update(ToolKind::YtDlp, true).await.unwrap());
    assert_eq!(f.current(ToolKind::YtDlp), YT2);
}

#[tokio::test]
async fn check_times_survive_a_restart_through_the_store() {
    let f = Fixture::new().await;
    let store = crate::store::Store::open(&f.tmp.path().join("t.db")).unwrap();
    let manager = manager_for(f.tmp.path(), &f.host).with_store(store.clone());
    manager.ensure(ToolKind::Deno, &|_| {}).await.unwrap();
    assert!(store.tool_checked_at("deno").unwrap().is_some());

    let again = manager_for(f.tmp.path(), &f.host).with_store(store);
    let before = f.host.requests("");
    assert!(!again.update(ToolKind::Deno, false, &|_| {}).await.unwrap());
    assert_eq!(f.host.requests(""), before);
}

#[tokio::test]
async fn offline_install_is_transient() {
    let f = Fixture::new().await;
    f.host.go_offline();
    let err = f
        .manager
        .ensure(ToolKind::YtDlp, &|_| {})
        .await
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::ConnectionLost, "{err}");
    assert!(err.kind.is_transient());
    assert_eq!(
        f.status_of(ToolKind::YtDlp).state,
        ToolState::Failed(ErrorKind::ConnectionLost)
    );

    // A failed check is not remembered: back online, it checks again.
    f.host.go_online();
    f.ensure(ToolKind::YtDlp).await;
    f.host.go_offline();
    let err = f.update(ToolKind::YtDlp, true).await.unwrap_err();
    assert!(err.kind.is_transient());
    assert_eq!(f.current(ToolKind::YtDlp), YT1);
}

#[tokio::test]
async fn a_failed_check_holds_unforced_checks_off() {
    let f = Fixture::new().await;
    f.ensure(ToolKind::YtDlp).await;
    // Due again (as if the install were days ago), but the host is gone.
    *lock(&f.manager.checked).get_mut(&ToolKind::YtDlp).unwrap() -= 2 * DAY_SECS;
    f.host.go_offline();
    let err = f.update(ToolKind::YtDlp, false).await.unwrap_err();
    assert!(err.kind.is_transient());
    // Back online: no request for an hour, unless forced.
    f.host.go_online();
    f.host.set_release("yt-dlp", ToolRelease::new(YT2));
    let before = f.host.requests("");
    assert!(!f.update(ToolKind::YtDlp, false).await.unwrap());
    assert_eq!(f.host.requests(""), before);
    assert!(f.update(ToolKind::YtDlp, true).await.unwrap());
    assert_eq!(f.current(ToolKind::YtDlp), YT2);
    // A check that worked lifts the hold.
    assert!(lock(&f.manager.check_backoff).is_empty());
}

#[tokio::test]
async fn a_withdrawn_build_keeps_the_installed_one() {
    let f = Fixture::new().await;
    f.ensure(ToolKind::YtDlp).await;
    f.host.set_release("yt-dlp", ToolRelease::new(YT2));
    let base = url::Url::parse(&f.host.base_url()).unwrap();
    let gone = ToolSources {
        yt_dlp: base.join("gone/").unwrap(),
        ..ToolSources::under(&base)
    };
    let manager = ToolManager::new(f.tmp.path(), gone.clone(), crate::http::build_client());
    let err = manager
        .update(ToolKind::YtDlp, true, &|_| {})
        .await
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::ToolMissing, "{err}");
    assert_eq!(f.current(ToolKind::YtDlp), YT1);
    assert_eq!(f.status_of(ToolKind::YtDlp).state, ToolState::Ready);
    // Not a network failure: the check counts, so the next one waits.
    let before = f.host.requests("");
    assert!(
        !manager
            .update(ToolKind::YtDlp, false, &|_| {})
            .await
            .unwrap()
    );
    assert_eq!(f.host.requests(""), before);

    let fresh = tempfile::tempdir().unwrap();
    let manager = ToolManager::new(fresh.path(), gone, crate::http::build_client());
    let err = manager.ensure(ToolKind::YtDlp, &|_| {}).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::ToolMissing);
}

#[test]
fn check_intervals() {
    let day = 24 * 60 * 60;
    assert!(check_due(ToolKind::YtDlp, None, 1_000));
    assert!(!check_due(ToolKind::YtDlp, Some(1_000), 1_000 + day - 1));
    assert!(check_due(ToolKind::YtDlp, Some(1_000), 1_000 + day));
    assert!(!check_due(
        ToolKind::Ffmpeg,
        Some(1_000),
        1_000 + 7 * day - 1
    ));
    assert!(check_due(ToolKind::Ffmpeg, Some(1_000), 1_000 + 7 * day));
    assert!(!check_due(ToolKind::Deno, Some(1_000), 1_000 + 6 * day));
    assert!(check_due(ToolKind::Deno, Some(1_000), 1_000 + 7 * day));
    // A clock set back does not postpone checks for good.
    assert!(check_due(ToolKind::YtDlp, Some(1_000), 10));
}

/// Every platform's layout installs (the fake host serves the native fake
/// binary under every asset name, so even foreign builds run here): macOS
/// ffmpeg through redirects and `.sha256` sidecars, Windows zips with
/// PowerShell sidecars, musl and arm builds.
#[tokio::test]
async fn every_platform_layout_installs() {
    let f = Fixture::new().await;
    let sources = ToolSources::under(&url::Url::parse(&f.host.base_url()).unwrap());
    let client = crate::http::build_client();
    for (kind, os, arch, musl) in [
        (ToolKind::Ffmpeg, "macos", "x86_64", false),
        (ToolKind::Ffmpeg, "macos", "aarch64", false),
        (ToolKind::Ffmpeg, "windows", "x86_64", false),
        (ToolKind::Ffmpeg, "windows", "aarch64", false),
        (ToolKind::Ffmpeg, "linux", "x86_64", false),
        (ToolKind::Ffmpeg, "linux", "aarch64", true),
        (ToolKind::Deno, "windows", "x86_64", false),
        (ToolKind::Deno, "windows", "aarch64", false),
        (ToolKind::Deno, "macos", "aarch64", false),
        (ToolKind::Deno, "linux", "aarch64", false),
        (ToolKind::YtDlp, "windows", "aarch64", false),
        (ToolKind::YtDlp, "macos", "x86_64", false),
        (ToolKind::YtDlp, "linux", "aarch64", true),
    ] {
        let label = format!("{kind:?} {os}/{arch} musl={musl}");
        let assets = catalog::asset_for(kind, os, arch, musl, &sources).unwrap();
        let names: Vec<&str> = assets
            .iter()
            .flat_map(|a| a.layout.install_names())
            .collect();
        let install = install::Install {
            client: &client,
            dir: f.tmp.path().join(format!("{kind:?}-{os}-{arch}-{musl}")),
            assets,
            version_arg: version_arg(kind),
            version_timeout: std::time::Duration::from_secs(10),
        };
        let expected = install.expected_version().await.unwrap();
        let tag = (kind != ToolKind::Ffmpeg).then(|| "1.0".to_owned());
        let version = install.run(tag, None, &|_, _| {}).await.unwrap();
        if kind == ToolKind::Ffmpeg {
            assert_eq!(version, expected, "{label}");
        }
        assert_eq!(install::read_current(&install.dir).unwrap(), version);
        for name in names {
            let path = install.dir.join(&version).join(name);
            let arg = version_arg(kind);
            assert!(!version_output(&path, arg).is_empty(), "{label}: {name}");
        }
    }
}
