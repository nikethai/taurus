//! A fake Hyades release host for the update check: a signed `latest.json`
//! and its assets, served from `127.0.0.1` by a [`TestServer`].
//!
//! The minisign key pair is generated when the host starts and lives only
//! in memory; no key is ever written to disk.

use minisign::KeyPair;
use url::Url;

use crate::{TestServer, sha256_hex};

const JSON: &str = "application/json";
const BINARY: &str = "application/octet-stream";
const NOTES: &str = "Short release notes, plain text.";

/// Serves `/latest.json`, `/latest.json.minisig` and
/// `/assets/<platform>.bin` (one per published platform).
pub struct FakeRelease {
    /// `http://127.0.0.1:<port>/latest.json`.
    pub manifest_url: Url,
    /// The minisign public key's base64 line.
    pub public_key: String,
    server: TestServer,
    /// The manifest as signed.
    manifest: Vec<u8>,
    assets: Vec<(String, Vec<u8>)>,
}

impl FakeRelease {
    /// Publishes `version` (written into the manifest exactly as given, so
    /// `"v1.1.0"` stays `"v1.1.0"`) with one asset per `(platform, bytes)`.
    pub async fn start(version: &str, assets: &[(&str, Vec<u8>)]) -> FakeRelease {
        let server = TestServer::start().await;
        let base = server.base_url().to_owned();
        let mut platforms = serde_json::Map::new();
        for (platform, bytes) in assets {
            let url = server.serve_bytes(&asset_path(platform), BINARY, bytes);
            platforms.insert(
                (*platform).to_owned(),
                serde_json::json!({
                    "url": url,
                    "size": bytes.len(),
                    "sha256": sha256_hex(bytes),
                }),
            );
        }
        let tag = version.trim_start_matches('v');
        let manifest = serde_json::to_vec_pretty(&serde_json::json!({
            "version": version,
            "pub_date": "2026-12-01T10:00:00Z",
            "notes": NOTES,
            "page": format!("https://github.com/nikethai/taurus/releases/tag/v{tag}"),
            "platforms": platforms,
        }))
        .expect("manifest JSON");

        let keys = KeyPair::generate_unencrypted_keypair().expect("generate test key pair");
        let signature = minisign::sign(Some(&keys.pk), &keys.sk, &manifest[..], None, None)
            .expect("sign the test manifest")
            .into_string();
        server.serve_bytes("/latest.json", JSON, &manifest);
        server.serve_bytes("/latest.json.minisig", "text/plain", signature.as_bytes());

        FakeRelease {
            manifest_url: Url::parse(&format!("{base}/latest.json")).expect("manifest URL"),
            public_key: keys.pk.to_base64(),
            server,
            manifest,
            assets: assets
                .iter()
                .map(|(p, b)| ((*p).to_owned(), b.clone()))
                .collect(),
        }
    }

    /// Serves the manifest with one byte of its notes changed after signing
    /// (still valid JSON); the signature stays the original one.
    pub fn tamper_manifest(&self) {
        let mut json = self.manifest.clone();
        let at = find(&json, NOTES.as_bytes()).expect("notes in the manifest");
        json[at] = b's'; // "Short" -> "short"
        self.server.serve_bytes("/latest.json", JSON, &json);
    }

    /// Answers `/latest.json` as a captive portal would: `200 text/html`.
    pub fn serve_html_manifest(&self) {
        self.server
            .serve_page("/latest.json", 200, &[], "<html>Sign in to Wi-Fi</html>");
    }

    /// Serves `platform`'s asset with the same length but different bytes,
    /// so it no longer matches the manifest's hash.
    pub fn corrupt_asset(&self, platform: &str) {
        let (_, bytes) = self
            .assets
            .iter()
            .find(|(p, _)| p == platform)
            .unwrap_or_else(|| panic!("corrupt_asset: no asset for {platform:?}"));
        let mut bad = bytes.clone();
        match bad.first_mut() {
            Some(b) => *b ^= 0xff,
            None => bad.push(0),
        }
        self.server.serve_bytes(&asset_path(platform), BINARY, &bad);
    }

    /// The underlying server, to serve something else at a path (e.g. an
    /// oversized `/latest.json`) or count requests.
    pub fn server(&self) -> &TestServer {
        &self.server
    }
}

fn asset_path(platform: &str) -> String {
    format!("/assets/{platform}.bin")
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}
