//! `cargo xtask verify-manifest <latest.json> <latest.json.minisig> <pub-file>`:
//! checks a signed manifest the way installed copies of Hyades do, so a
//! release never ships a `latest.json` they would reject.

use std::fs;
use std::path::Path;

pub fn run(args: &[String]) -> Result<(), String> {
    let [json, sig, public] = args else {
        return Err(
            "usage: cargo xtask verify-manifest <latest.json> <latest.json.minisig> <pub-file>"
                .into(),
        );
    };
    let summary = verify_files(Path::new(json), Path::new(sig), Path::new(public))?;
    println!("{json} verifies against {public}: {summary}");
    Ok(())
}

/// Verifies `json` with signature file `sig` against public key file
/// `public` (a comment line, then the key's base64 line, as in
/// `packaging/update-key.pub`). Returns a short summary of the manifest.
pub fn verify_files(json: &Path, sig: &Path, public: &Path) -> Result<String, String> {
    let read = |p: &Path| fs::read(p).map_err(|e| format!("{}: {e}", p.display()));
    let json_bytes = read(json)?;
    let sig_text = String::from_utf8(read(sig)?)
        .map_err(|_| format!("{}: not a minisign signature", sig.display()))?;
    let key_text = String::from_utf8(read(public)?)
        .map_err(|_| format!("{}: not a minisign public key", public.display()))?;
    // The app reads the key the same way (update_flow::public_key).
    let key = key_text.lines().nth(1).unwrap_or_default().trim();
    match taurus_core::update::verify_manifest(&json_bytes, &sig_text, key) {
        Ok(m) => Ok(format!(
            "version {}, {} platform(s)",
            m.version,
            m.platforms.len()
        )),
        Err(e) => {
            let mut msg = format!(
                "{} does not verify against {} ({e}); installed copies would reject this update. \
                 The signing key (HYADES_MINISIGN_KEY) must be the secret half of that public key.",
                json.display(),
                public.display()
            );
            if key_text
                .lines()
                .next()
                .is_some_and(|l| l.contains("placeholder"))
            {
                msg.push_str(
                    " The public key is still the placeholder: run `cargo xtask keygen` and commit the new key.",
                );
            }
            Err(msg)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use minisign::KeyPair;

    const MANIFEST: &str =
        r#"{"version":"1.2.3","notes":"","page":"https://example.org/r","platforms":{}}"#;

    /// Writes `latest.json`, its signature by a fresh key, and that key's
    /// public file; returns the folder.
    fn signed() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let pair = KeyPair::generate_unencrypted_keypair().unwrap();
        let secret = pair.sk.to_box(None).unwrap().into_string();
        let sig =
            crate::sign::sign_bytes(&secret, None, MANIFEST.as_bytes(), "latest.json").unwrap();
        fs::write(tmp.path().join("latest.json"), MANIFEST).unwrap();
        fs::write(tmp.path().join("latest.json.minisig"), sig).unwrap();
        let public = format!("untrusted comment: test key\n{}\n", pair.pk.to_base64());
        fs::write(tmp.path().join("good.pub"), public).unwrap();
        tmp
    }

    fn verify(dir: &Path, public: &str) -> Result<String, String> {
        verify_files(
            &dir.join("latest.json"),
            &dir.join("latest.json.minisig"),
            &dir.join(public),
        )
    }

    #[test]
    fn the_signing_key_verifies() {
        let tmp = signed();
        let summary = verify(tmp.path(), "good.pub").unwrap();
        assert!(summary.contains("1.2.3"), "{summary}");
    }

    #[test]
    fn another_key_fails_with_a_clear_message() {
        let tmp = signed();
        let other = KeyPair::generate_unencrypted_keypair().unwrap();
        let text = format!("untrusted comment: other\n{}\n", other.pk.to_base64());
        fs::write(tmp.path().join("other.pub"), text).unwrap();
        let err = verify(tmp.path(), "other.pub").unwrap_err();
        assert!(err.contains("does not verify"), "{err}");
        assert!(!err.contains("placeholder"), "{err}");
    }

    #[test]
    fn the_committed_placeholder_key_fails_and_says_so() {
        let tmp = signed();
        let repo_key = crate::repo_root().join("packaging/update-key.pub");
        let text = fs::read_to_string(&repo_key).unwrap();
        if !text.contains("placeholder") {
            return; // the release key is in place
        }
        fs::write(tmp.path().join("placeholder.pub"), text).unwrap();
        let err = verify(tmp.path(), "placeholder.pub").unwrap_err();
        assert!(err.contains("cargo xtask keygen"), "{err}");
    }

    #[test]
    fn a_changed_manifest_fails() {
        let tmp = signed();
        fs::write(
            tmp.path().join("latest.json"),
            MANIFEST.replace("1.2.3", "9.9.9"),
        )
        .unwrap();
        assert!(verify(tmp.path(), "good.pub").is_err());
    }
}
