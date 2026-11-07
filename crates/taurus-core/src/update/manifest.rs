//! `latest.json`: its signature, then its contents.

use std::collections::BTreeMap;

use minisign_verify::{PublicKey, Signature};
use serde::Deserialize;
use url::Url;

use super::UpdateError;

/// The largest `latest.json` read (1 MiB).
pub(super) const MANIFEST_CAP: u64 = 1024 * 1024;
/// The largest `latest.json.minisig` read (4 KiB).
pub(super) const SIGNATURE_CAP: u64 = 4 * 1024;

/// A published release, as `latest.json` describes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Manifest {
    pub version: semver::Version,
    pub pub_date: Option<String>,
    pub notes: String,
    /// The release's web page.
    pub page: Url,
    /// By platform key (see [`super::platform_key`]).
    pub platforms: BTreeMap<String, Asset>,
}

/// One downloadable file of a release.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Asset {
    pub url: Url,
    pub size: u64,
    /// Lowercase or uppercase hex of the file's SHA-256.
    pub sha256: String,
}

#[derive(Deserialize)]
struct RawManifest {
    version: String,
    #[serde(default)]
    pub_date: Option<String>,
    #[serde(default)]
    notes: String,
    page: String,
    #[serde(default)]
    platforms: BTreeMap<String, RawAsset>,
}

#[derive(Deserialize)]
struct RawAsset {
    url: String,
    size: u64,
    sha256: String,
}

/// Checks `minisig` (the text of `latest.json.minisig`) over `json` with
/// `public_key` (a minisign public key's base64 line), and only then parses
/// `json`. Any signature problem, an unusable key included, is
/// `BadSignature`; a signed but unreadable manifest is `BadManifest`.
pub fn verify_manifest(
    json: &[u8],
    minisig: &str,
    public_key: &str,
) -> Result<Manifest, UpdateError> {
    let key = PublicKey::from_base64(public_key.trim()).map_err(|_| UpdateError::BadSignature)?;
    let signature = Signature::decode(minisig).map_err(|_| UpdateError::BadSignature)?;
    // Prehashed signatures only (what minisign makes by default).
    key.verify(json, &signature, false)
        .map_err(|_| UpdateError::BadSignature)?;
    parse(json)
}

fn parse(json: &[u8]) -> Result<Manifest, UpdateError> {
    let raw: RawManifest =
        serde_json::from_slice(json).map_err(|e| bad(format!("not a manifest: {e}")))?;
    let version = parse_version(&raw.version)?;
    let page = parse_url(&raw.page, "page")?;
    let platforms = raw
        .platforms
        .into_iter()
        .map(|(key, asset)| {
            let url = parse_url(&asset.url, &key)?;
            let asset = Asset {
                url,
                size: asset.size,
                sha256: asset.sha256,
            };
            Ok((key, asset))
        })
        .collect::<Result<_, UpdateError>>()?;
    Ok(Manifest {
        version,
        pub_date: raw.pub_date,
        notes: raw.notes,
        page,
        platforms,
    })
}

/// A semantic version, with or without a leading `v`.
fn parse_version(text: &str) -> Result<semver::Version, UpdateError> {
    let text = text.strip_prefix('v').unwrap_or(text);
    semver::Version::parse(text).map_err(|e| bad(format!("version {text:?}: {e}")))
}

fn parse_url(text: &str, what: &str) -> Result<Url, UpdateError> {
    Url::parse(text).map_err(|e| bad(format!("{what} URL: {e}")))
}

fn bad(detail: String) -> UpdateError {
    UpdateError::BadManifest(detail)
}
