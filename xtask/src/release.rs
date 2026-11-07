//! `cargo xtask manifest`: `latest.json` and `SHA256SUMS` for a release's
//! `dist` directory.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use sha2::{Digest, Sha256};

/// Platform key and the asset's name with `{v}` for the version.
const PLATFORMS: [(&str, &str); 5] = [
    ("windows-x86_64", "Hyades_{v}_x64-setup.exe"),
    ("windows-aarch64", "Hyades_{v}_arm64-setup.exe"),
    ("macos-universal", "Hyades_{v}_universal.app.tar.gz"),
    ("linux-x86_64-appimage", "Hyades_{v}_x86_64.AppImage"),
    ("linux-aarch64-appimage", "Hyades_{v}_aarch64.AppImage"),
];

pub struct Options {
    pub version: String,
    pub dist: String,
    pub base_url: String,
    pub notes: String,
    pub allow_missing: bool,
}

pub fn run(args: &[String]) -> Result<(), String> {
    let usage = "usage: cargo xtask manifest --version <v> --dist <dir> --base-url <url> --notes <file> [--allow-missing]";
    let (mut version, mut dist, mut base_url, mut notes) = (None, None, None, None);
    let mut allow_missing = false;
    let mut it = args.iter();
    while let Some(flag) = it.next() {
        let mut value = || it.next().cloned().ok_or_else(|| usage.to_string());
        match flag.as_str() {
            "--version" => version = Some(value()?),
            "--dist" => dist = Some(value()?),
            "--base-url" => base_url = Some(value()?),
            "--notes" => notes = Some(value()?),
            "--allow-missing" => allow_missing = true,
            _ => return Err(usage.into()),
        }
    }
    let (Some(version), Some(dist), Some(base_url), Some(notes)) = (version, dist, base_url, notes)
    else {
        return Err(usage.into());
    };
    let opts = Options {
        version,
        dist,
        base_url,
        notes,
        allow_missing,
    };
    let notes_text = fs::read_to_string(&opts.notes).map_err(|e| format!("{}: {e}", opts.notes))?;
    write_release(&opts, &notes_text)?;
    println!("wrote latest.json and SHA256SUMS in {}", opts.dist);
    Ok(())
}

pub fn write_release(opts: &Options, notes: &str) -> Result<(), String> {
    let dist = Path::new(&opts.dist);
    let json = manifest_json(opts, notes)?;
    fs::write(dist.join("latest.json"), json).map_err(|e| e.to_string())?;
    fs::write(dist.join("SHA256SUMS"), sha256sums(dist)?).map_err(|e| e.to_string())
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// The pretty-printed manifest. Asset URLs are `<base_url><asset name>`.
pub fn manifest_json(opts: &Options, notes: &str) -> Result<Vec<u8>, String> {
    let version = opts.version.trim_start_matches('v');
    let base = if opts.base_url.ends_with('/') {
        opts.base_url.clone()
    } else {
        format!("{}/", opts.base_url)
    };
    let dist = Path::new(&opts.dist);
    let mut platforms = BTreeMap::new();
    for (key, pattern) in PLATFORMS {
        let name = pattern.replace("{v}", version);
        let path = dist.join(&name);
        let bytes = match fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && opts.allow_missing => continue,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(format!(
                    "missing asset for {key}: {name} (use --allow-missing to skip)"
                ));
            }
            Err(e) => return Err(format!("{}: {e}", path.display())),
        };
        platforms.insert(
            key,
            serde_json::json!({
                "url": format!("{base}{name}"),
                "size": bytes.len(),
                "sha256": sha256_hex(&bytes),
            }),
        );
    }
    serde_json::to_vec_pretty(&serde_json::json!({
        "version": version,
        "notes": notes,
        "page": format!("https://github.com/nikethai/taurus/releases/tag/v{version}"),
        "platforms": platforms,
    }))
    .map_err(|e| e.to_string())
}

/// `sha256sum` format, sorted by name, for every release file in `dist`.
pub fn sha256sums(dist: &Path) -> Result<String, String> {
    let mut names = Vec::new();
    for entry in fs::read_dir(dist).map_err(|e| format!("{}: {e}", dist.display()))? {
        let entry = entry.map_err(|e| e.to_string())?;
        if !entry.file_type().map_err(|e| e.to_string())?.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with("SHA256SUMS")
            || name.starts_with("latest.json")
            || name.ends_with(".minisig")
        {
            continue;
        }
        names.push(name);
    }
    names.sort();
    let mut out = String::new();
    for name in names {
        let bytes = fs::read(dist.join(&name)).map_err(|e| format!("{name}: {e}"))?;
        out.push_str(&format!("{}  {name}\n", sha256_hex(&bytes)));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use minisign::KeyPair;

    fn dist_with_assets(version: &str, skip: Option<usize>) -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        for (i, (_, pattern)) in PLATFORMS.iter().enumerate() {
            if skip == Some(i) {
                continue;
            }
            let name = pattern.replace("{v}", version);
            fs::write(tmp.path().join(name), format!("asset {i}")).unwrap();
        }
        tmp
    }

    fn opts(dist: &Path, allow_missing: bool) -> Options {
        Options {
            version: "1.2.3".into(),
            dist: dist.to_str().unwrap().into(),
            base_url: "https://example.org/dl".into(),
            notes: String::new(),
            allow_missing,
        }
    }

    #[test]
    fn manifest_lists_present_assets() {
        let tmp = dist_with_assets("1.2.3", None);
        let json = manifest_json(&opts(tmp.path(), false), "notes").unwrap();
        let v: serde_json::Value = serde_json::from_slice(&json).unwrap();
        assert_eq!(v["version"], "1.2.3");
        assert_eq!(
            v["page"],
            "https://github.com/nikethai/taurus/releases/tag/v1.2.3"
        );
        let platforms = v["platforms"].as_object().unwrap();
        assert_eq!(platforms.len(), 5);
        for (i, (key, pattern)) in PLATFORMS.iter().enumerate() {
            let content = format!("asset {i}");
            let name = pattern.replace("{v}", "1.2.3");
            let p = &platforms[*key];
            assert_eq!(p["size"], content.len());
            assert_eq!(p["sha256"], sha256_hex(content.as_bytes()));
            assert_eq!(p["url"], format!("https://example.org/dl/{name}"));
        }
    }

    #[test]
    fn manifest_missing_asset_is_error() {
        let tmp = dist_with_assets("1.2.3", Some(2));
        let err = manifest_json(&opts(tmp.path(), false), "").unwrap_err();
        assert!(err.contains("macos-universal"), "{err}");
        let json = manifest_json(&opts(tmp.path(), true), "").unwrap();
        let v: serde_json::Value = serde_json::from_slice(&json).unwrap();
        assert_eq!(v["platforms"].as_object().unwrap().len(), 4);
        assert!(v["platforms"].get("macos-universal").is_none());
    }

    #[test]
    fn sha256sums_format() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("b.bin"), "b").unwrap();
        fs::write(tmp.path().join("a.bin"), "a").unwrap();
        for skipped in [
            "SHA256SUMS",
            "SHA256SUMS.minisig",
            "latest.json",
            "latest.json.minisig",
            "a.bin.minisig",
        ] {
            fs::write(tmp.path().join(skipped), "x").unwrap();
        }
        let sums = sha256sums(tmp.path()).unwrap();
        assert_eq!(
            sums,
            format!("{}  a.bin\n{}  b.bin\n", sha256_hex(b"a"), sha256_hex(b"b"))
        );
    }

    #[test]
    fn signed_manifest_verifies_with_core() {
        let tmp = dist_with_assets("1.2.3", None);
        write_release(&opts(tmp.path(), false), "notes").unwrap();
        let json = fs::read(tmp.path().join("latest.json")).unwrap();
        let pair = KeyPair::generate_unencrypted_keypair().unwrap();
        let secret = pair.sk.to_box(None).unwrap().into_string();
        let sig = crate::sign::sign_bytes(&secret, None, &json, "latest.json").unwrap();
        let manifest =
            taurus_core::update::verify_manifest(&json, &sig, &pair.pk.to_base64()).unwrap();
        assert_eq!(manifest.platforms.len(), 5);
        let key = taurus_core::update::platform_key("windows", "x86_64", false).unwrap();
        assert!(manifest.platforms.contains_key(&key));
    }
}
