//! Hyades' own updates: the check against a signed `latest.json`, and the
//! download of the platform's asset, verified by size and SHA-256.
//!
//! `latest.json` is signed as a whole with minisign; its signature is
//! fetched from the same URL plus `.minisig` and checked before the JSON is
//! parsed. The asset's size and hash come from that signed manifest, so a
//! downloaded file that matches them is the published one. Callers pass a
//! client from [`crate::network::build_client`], so the proxy setting applies.

mod fetch;
mod manifest;
#[cfg(test)]
mod tests;

use std::path::{Path, PathBuf};

use url::Url;

pub use manifest::{Asset, Manifest, verify_manifest};
use manifest::{MANIFEST_CAP, SIGNATURE_CAP};

/// Where and how to look for an update.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpdateConfig {
    /// `latest.json`; its signature is this URL plus `.minisig`.
    pub manifest_url: Url,
    /// The minisign public key's base64 line.
    pub public_key: String,
    /// This build's platform key ([`platform_key`]); `None` when no asset
    /// fits (the release is then only reported).
    pub platform: Option<String>,
    /// The running version.
    pub current: semver::Version,
}

/// A release newer than the running one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Release {
    pub version: semver::Version,
    pub notes: String,
    pub page: Url,
    /// The asset for this platform, if the release has one.
    pub asset: Option<Asset>,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum UpdateError {
    /// No answer, or an HTTP error status; the detail says which.
    #[error("update server unreachable: {0}")]
    Offline(String),
    /// The manifest is not signed by the expected key (a captive portal's
    /// page included).
    #[error("the update manifest's signature is not valid")]
    BadSignature,
    /// The manifest is too large, unreadable, or names an asset that is
    /// not downloaded.
    #[error("bad update manifest: {0}")]
    BadManifest(String),
    /// The downloaded file's size or SHA-256 differs from the manifest's.
    #[error("the downloaded update does not match the manifest")]
    Mismatch,
    #[error("cannot write the update: {0}")]
    Disk(String),
}

/// The manifest key of a build for `os` and `arch` (Rust's
/// `std::env::consts` names); `appimage` when a Linux build runs as an
/// AppImage, the only Linux form that updates itself.
pub fn platform_key(os: &str, arch: &str, appimage: bool) -> Option<String> {
    match os {
        "windows" => Some(format!("windows-{arch}")),
        "macos" => Some("macos-universal".to_owned()),
        "linux" if appimage => Some(format!("linux-{arch}-appimage")),
        _ => None,
    }
}

/// Fetches and verifies the manifest; `Some` when it offers a release newer
/// than `cfg.current`. A prerelease is offered only to a prerelease.
pub async fn check(
    client: &reqwest::Client,
    cfg: &UpdateConfig,
) -> Result<Option<Release>, UpdateError> {
    let json = fetch::get_capped(client, &cfg.manifest_url, MANIFEST_CAP).await?;
    let signature_url = signature_url(&cfg.manifest_url);
    let signature = fetch::get_capped(client, &signature_url, SIGNATURE_CAP).await?;
    let signature = String::from_utf8(signature).map_err(|_| UpdateError::BadSignature)?;
    let manifest = verify_manifest(&json, &signature, &cfg.public_key)?;
    if manifest.version <= cfg.current
        || (!manifest.version.pre.is_empty() && cfg.current.pre.is_empty())
    {
        return Ok(None);
    }
    let asset = cfg
        .platform
        .as_ref()
        .and_then(|key| manifest.platforms.get(key))
        .cloned();
    Ok(Some(Release {
        version: manifest.version,
        notes: manifest.notes,
        page: manifest.page,
        asset,
    }))
}

/// Downloads `asset` to `dir/<name>` (`name`: the URL's last path segment)
/// and answers that path. The bytes go to `dir/<name>.part`, started from
/// scratch; it becomes `dir/<name>` (replacing an older file) only when its
/// size and SHA-256 match `asset`, and is removed otherwise (`Mismatch`) or
/// on any other error. `progress(done, total)` follows the bytes received.
///
/// The URL must be `https` (or `http` to a loopback host), the size at most
/// 500 MiB and the name non-empty without `..`; otherwise `BadManifest`,
/// before any request.
pub async fn download(
    client: &reqwest::Client,
    asset: &Asset,
    dir: &Path,
    progress: impl Fn(u64, u64) + Send,
) -> Result<PathBuf, UpdateError> {
    fetch::download(client, asset, dir, progress).await
}

/// The manifest URL with `.minisig` added to its path.
fn signature_url(manifest_url: &Url) -> Url {
    let mut url = manifest_url.clone();
    url.set_path(&format!("{}.minisig", manifest_url.path()));
    url
}
