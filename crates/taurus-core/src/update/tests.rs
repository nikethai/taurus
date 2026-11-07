use std::path::Path;

use sha2::Digest;
use taurus_testkit::{FakeRelease, TestServer};

use super::*;

const PLATFORM: &str = "linux-x86_64-appimage";

fn client() -> reqwest::Client {
    crate::network::build_client(&crate::ProxySetting::None)
}

fn config(host: &FakeRelease, current: &str, platform: Option<&str>) -> UpdateConfig {
    UpdateConfig {
        manifest_url: host.manifest_url.clone(),
        public_key: host.public_key.clone(),
        platform: platform.map(str::to_owned),
        current: semver::Version::parse(current).unwrap(),
    }
}

fn asset_bytes() -> Vec<u8> {
    (0..200_000u32).map(|i| (i % 251) as u8).collect()
}

async fn host(version: &str) -> FakeRelease {
    FakeRelease::start(version, &[(PLATFORM, asset_bytes())]).await
}

fn files_in(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect()
}

#[tokio::test]
async fn newer_version_is_offered() {
    let host = host("1.1.0").await;
    let r = check(&client(), &config(&host, "1.0.0", Some(PLATFORM)))
        .await
        .unwrap()
        .expect("a release is offered");
    assert!(r.asset.is_some());
    assert_eq!(r.version, semver::Version::new(1, 1, 0));
}

#[tokio::test]
async fn same_or_older_is_not() {
    for version in ["1.0.0", "0.9.0"] {
        let host = host(version).await;
        let r = check(&client(), &config(&host, "1.0.0", Some(PLATFORM))).await;
        assert_eq!(r, Ok(None), "host at {version}");
    }
}

#[tokio::test]
async fn prerelease_ignored_on_stable() {
    let host = host("1.1.0-rc.1").await;
    let stable = check(&client(), &config(&host, "1.0.0", Some(PLATFORM))).await;
    assert_eq!(stable, Ok(None));
    let pre = check(&client(), &config(&host, "1.0.0-rc.1", Some(PLATFORM))).await;
    assert!(matches!(pre, Ok(Some(_))), "{pre:?}");
}

#[tokio::test]
async fn leading_v_accepted() {
    let host = host("v1.1.0").await;
    let r = check(&client(), &config(&host, "1.0.0", Some(PLATFORM))).await;
    let r = r.unwrap().expect("a release is offered");
    assert_eq!(r.version, semver::Version::new(1, 1, 0));
}

#[tokio::test]
async fn platform_without_asset_still_reports_release() {
    let host = host("1.1.0").await;
    let r = check(&client(), &config(&host, "1.0.0", None))
        .await
        .unwrap()
        .expect("a release is offered");
    assert_eq!(r.asset, None);
    assert_eq!(
        r.page.as_str(),
        "https://github.com/nikethai/taurus/releases/tag/v1.1.0"
    );
}

#[tokio::test]
async fn tampered_manifest_rejected() {
    let host = host("1.1.0").await;
    host.tamper_manifest();
    let r = check(&client(), &config(&host, "1.0.0", Some(PLATFORM))).await;
    assert_eq!(r, Err(UpdateError::BadSignature));
}

#[tokio::test]
async fn captive_portal_page_is_refused() {
    let host = host("1.1.0").await;
    host.serve_html_manifest();
    let r = check(&client(), &config(&host, "1.0.0", Some(PLATFORM))).await;
    assert_eq!(r, Err(UpdateError::BadSignature));
}

#[tokio::test]
async fn oversized_manifest_rejected() {
    let host = host("1.1.0").await;
    let body = vec![b' '; 2 * 1024 * 1024];
    host.server()
        .serve_bytes("/latest.json", "application/json", &body);
    let r = check(&client(), &config(&host, "1.0.0", Some(PLATFORM))).await;
    assert!(matches!(r, Err(UpdateError::BadManifest(_))), "{r:?}");
}

#[tokio::test]
async fn download_checks_size_and_hash() {
    let host = host("1.1.0").await;
    let r = check(&client(), &config(&host, "1.0.0", Some(PLATFORM)))
        .await
        .unwrap()
        .unwrap();
    host.corrupt_asset(PLATFORM);
    let dir = tempfile::tempdir().unwrap();
    let got = download(&client(), &r.asset.unwrap(), dir.path(), |_, _| {}).await;
    assert_eq!(got, Err(UpdateError::Mismatch));
    assert_eq!(files_in(dir.path()), Vec::<String>::new());
}

#[tokio::test]
async fn stale_partial_is_discarded() {
    let host = host("1.1.0").await;
    let r = check(&client(), &config(&host, "1.0.0", Some(PLATFORM)))
        .await
        .unwrap()
        .unwrap();
    let asset = r.asset.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let name = format!("{PLATFORM}.bin");
    std::fs::write(dir.path().join(format!("{name}.part")), vec![7u8; 300_000]).unwrap();
    let seen = std::sync::Mutex::new((0u64, 0u64));
    let path = download(&client(), &asset, dir.path(), |done, total| {
        *seen.lock().unwrap() = (done, total);
    })
    .await
    .unwrap();
    assert_eq!(path, dir.path().join(&name));
    assert_eq!(std::fs::read(&path).unwrap(), asset_bytes());
    assert_eq!(files_in(dir.path()), vec![name]);
    let total = asset_bytes().len() as u64;
    assert_eq!(*seen.lock().unwrap(), (total, total));
}

#[tokio::test]
async fn download_refuses_plain_http_remote() {
    // Every request goes through a recording proxy: none may reach it.
    let proxy = TestServer::start_proxy().await;
    let client = crate::network::build_client(&crate::ProxySetting::Manual {
        url: proxy.base_url().to_owned(),
    });
    let asset = Asset {
        url: Url::parse("http://example.com/x").unwrap(),
        size: 10,
        sha256: "0".repeat(64),
    };
    let dir = tempfile::tempdir().unwrap();
    let got = download(&client, &asset, dir.path(), |_, _| {}).await;
    assert!(matches!(got, Err(UpdateError::BadManifest(_))), "{got:?}");
    assert_eq!(proxy.proxied_requests(), Vec::<String>::new());
    assert_eq!(files_in(dir.path()), Vec::<String>::new());
}

#[test]
fn platform_key_table() {
    let key = |os, arch, appimage| platform_key(os, arch, appimage);
    assert_eq!(
        key("windows", "x86_64", false).as_deref(),
        Some("windows-x86_64")
    );
    assert_eq!(
        key("windows", "aarch64", false).as_deref(),
        Some("windows-aarch64")
    );
    assert_eq!(
        key("macos", "aarch64", false).as_deref(),
        Some("macos-universal")
    );
    assert_eq!(
        key("linux", "x86_64", true).as_deref(),
        Some("linux-x86_64-appimage")
    );
    assert_eq!(
        key("linux", "aarch64", true).as_deref(),
        Some("linux-aarch64-appimage")
    );
    assert_eq!(key("linux", "x86_64", false), None);
}

/// Answers every connection with `200` and `body` without Content-Length
/// (the body ends when the connection closes), so only a cap checked while
/// reading applies. Returns `http://127.0.0.1:<port>/`.
async fn serve_unsized(body: Vec<u8>) -> Url {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let body = body.clone();
            tokio::spawn(async move {
                let mut request = [0u8; 4096];
                let _ = socket.read(&mut request).await;
                let head = "HTTP/1.1 200 OK\r\nconnection: close\r\n\r\n";
                let _ = socket.write_all(head.as_bytes()).await;
                let _ = socket.write_all(&body).await;
            });
        }
    });
    url
}

#[tokio::test]
async fn manifest_cap_holds_without_content_length() {
    let url = serve_unsized(vec![b' '; 2 * 1024 * 1024]).await;
    let cfg = UpdateConfig {
        manifest_url: url.join("latest.json").unwrap(),
        public_key: String::new(),
        platform: None,
        current: semver::Version::new(1, 0, 0),
    };
    let r = check(&client(), &cfg).await;
    assert!(matches!(r, Err(UpdateError::BadManifest(_))), "{r:?}");
}

#[tokio::test]
async fn asset_size_holds_without_content_length() {
    let url = serve_unsized(vec![1u8; 256 * 1024]).await;
    let asset = Asset {
        url: url.join("x.bin").unwrap(),
        size: 1000,
        sha256: hex::encode(sha2::Sha256::digest([1u8; 1000])),
    };
    let dir = tempfile::tempdir().unwrap();
    let most = std::sync::atomic::AtomicU64::new(0);
    let got = download(&client(), &asset, dir.path(), |done, _| {
        most.fetch_max(done, std::sync::atomic::Ordering::Relaxed);
    })
    .await;
    assert_eq!(got, Err(UpdateError::Mismatch));
    // The read stopped at the first chunk past the size, before reporting it.
    assert!(most.into_inner() <= 1000);
    assert_eq!(files_in(dir.path()), Vec::<String>::new());
}
