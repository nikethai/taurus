//! The update's web requests: the small capped reads of the manifest and
//! its signature, and the verified download of an asset.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use url::{Host, Url};

use super::{Asset, UpdateError};
use crate::http::client::{request_error, status_error};
use crate::http::rename_with_retry;

/// The largest asset downloaded (500 MiB).
pub(super) const ASSET_CAP: u64 = 500 * 1024 * 1024;

/// GETs `url` and answers its body, refused with `BadManifest` as soon as
/// it is known to exceed `cap` bytes (from Content-Length, or while
/// reading: the header alone is not trusted).
pub(super) async fn get_capped(
    client: &reqwest::Client,
    url: &Url,
    cap: u64,
) -> Result<Vec<u8>, UpdateError> {
    let mut response = send(client, url).await?;
    let too_big = || UpdateError::BadManifest(format!("{url} is larger than {cap} bytes"));
    if response.content_length().is_some_and(|n| n > cap) {
        return Err(too_big());
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(offline)? {
        if (body.len() + chunk.len()) as u64 > cap {
            return Err(too_big());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// Downloads `asset` into `dir` and answers the file's path. See
/// [`super::download`].
pub(super) async fn download(
    client: &reqwest::Client,
    asset: &Asset,
    dir: &Path,
    progress: impl Fn(u64, u64) + Send,
) -> Result<PathBuf, UpdateError> {
    let name = check_asset(asset)?;
    let path = dir.join(name);
    let part = dir.join(format!("{name}.part"));
    tokio::fs::create_dir_all(dir).await.map_err(disk)?;
    remove_if_present(&part).await?;
    let result = fetch_into(client, asset, &part, progress).await;
    if let Err(e) = result {
        let _ = tokio::fs::remove_file(&part).await;
        return Err(e);
    }
    if let Err(e) = rename_with_retry(&part, &path).await {
        let _ = tokio::fs::remove_file(&part).await;
        return Err(UpdateError::Disk(e.detail));
    }
    Ok(path)
}

/// The asset's file name, if its URL, size and hash are ones to download:
/// `https` (or `http` to a loopback host), at most [`ASSET_CAP`] bytes, a
/// 64-digit hex SHA-256, and a last path segment usable as a file name.
fn check_asset(asset: &Asset) -> Result<&str, UpdateError> {
    let bad = |why: &str| {
        Err(UpdateError::BadManifest(format!(
            "asset {}: {why}",
            asset.url
        )))
    };
    let url = &asset.url;
    let loopback = match url.host() {
        Some(Host::Ipv4(ip)) => ip.is_loopback(),
        Some(Host::Ipv6(ip)) => ip.is_loopback(),
        Some(Host::Domain(d)) => d.eq_ignore_ascii_case("localhost"),
        None => false,
    };
    if !(url.scheme() == "https" || (url.scheme() == "http" && loopback)) {
        return bad("only https (or http to this computer) is downloaded");
    }
    if asset.size > ASSET_CAP {
        return bad("larger than 500 MiB");
    }
    if asset.sha256.len() != 64 || !asset.sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
        return bad("the SHA-256 is not 64 hex digits");
    }
    let name = url
        .path_segments()
        .and_then(|mut segments| segments.next_back())
        .unwrap_or("");
    // `..` and separators could leave `dir`; `:` names a stream on Windows.
    let unsafe_char = |c: char| matches!(c, '/' | '\\' | ':') || c.is_control();
    if name.is_empty() || name.contains("..") || name.contains(unsafe_char) {
        return bad("the URL names no usable file name");
    }
    Ok(name)
}

/// Writes the asset to `part` from scratch, hashing as it goes; `Mismatch`
/// as soon as more than `asset.size` bytes arrive, or at the end if the
/// size or hash differ. The caller removes `part` on error.
async fn fetch_into(
    client: &reqwest::Client,
    asset: &Asset,
    part: &Path,
    progress: impl Fn(u64, u64) + Send,
) -> Result<(), UpdateError> {
    let mut response = send(client, &asset.url).await?;
    if response.content_length().is_some_and(|n| n > asset.size) {
        return Err(UpdateError::Mismatch);
    }
    let mut file = tokio::fs::File::create(part).await.map_err(disk)?;
    let mut hasher = Sha256::new();
    let mut done = 0u64;
    progress(0, asset.size);
    while let Some(chunk) = response.chunk().await.map_err(offline)? {
        done += chunk.len() as u64;
        if done > asset.size {
            return Err(UpdateError::Mismatch);
        }
        hasher.update(&chunk);
        file.write_all(&chunk).await.map_err(disk)?;
        progress(done, asset.size);
    }
    file.flush().await.map_err(disk)?;
    file.sync_all().await.map_err(disk)?;
    drop(file);
    let hash = hex::encode(hasher.finalize());
    if done != asset.size || !hash.eq_ignore_ascii_case(&asset.sha256) {
        return Err(UpdateError::Mismatch);
    }
    Ok(())
}

/// Sends a GET; a transport error or a status other than 2xx is `Offline`.
async fn send(client: &reqwest::Client, url: &Url) -> Result<reqwest::Response, UpdateError> {
    let response = client.get(url.clone()).send().await.map_err(offline)?;
    let status = response.status();
    if !status.is_success() {
        return Err(UpdateError::Offline(status_error(status).detail));
    }
    Ok(response)
}

async fn remove_if_present(path: &Path) -> Result<(), UpdateError> {
    match tokio::fs::remove_file(path).await {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(disk(e)),
        _ => Ok(()),
    }
}

fn offline(e: reqwest::Error) -> UpdateError {
    UpdateError::Offline(request_error(e).detail)
}

fn disk(e: std::io::Error) -> UpdateError {
    UpdateError::Disk(e.to_string())
}
