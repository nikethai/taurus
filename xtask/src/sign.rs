//! `cargo xtask keygen` and `cargo xtask sign`: the minisign key for update
//! manifests. Secrets come from the environment or a prompt and are never
//! printed; errors here are fixed texts that cannot quote them.

use std::fs;
use std::path::{Path, PathBuf};

use minisign::{KeyPair, SecretKeyBox};

pub const PUBLIC_COMMENT: &str = "Hyades release key";

/// `sign <file>...`: writes `<file>.minisig` next to each file.
pub fn run_sign(args: &[String]) -> Result<(), String> {
    if args.is_empty() {
        return Err("usage: cargo xtask sign <file>...".into());
    }
    let secret = std::env::var("HYADES_MINISIGN_KEY")
        .map_err(|_| "HYADES_MINISIGN_KEY is not set".to_string())?;
    let password = std::env::var("HYADES_MINISIGN_PASSWORD").ok();
    for arg in args {
        let path = Path::new(arg);
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| format!("{arg}: not a file name"))?;
        let data = fs::read(path).map_err(|e| format!("{arg}: {e}"))?;
        let signature = sign_bytes(&secret, password.as_deref(), &data, name)?;
        let out = format!("{arg}.minisig");
        fs::write(&out, signature).map_err(|e| format!("{out}: {e}"))?;
        println!("signed {name}");
    }
    Ok(())
}

/// The `.minisig` text for `data`, with trusted comment `hyades <file_name>`.
/// Every error is a fixed text: it never contains the key or the password.
pub fn sign_bytes(
    secret: &str,
    password: Option<&str>,
    data: &[u8],
    file_name: &str,
) -> Result<String, String> {
    const BAD_KEY: &str = "cannot use the signing key (malformed, or wrong password)";
    let sk_box = SecretKeyBox::from_string(secret).map_err(|_| BAD_KEY.to_string())?;
    let sk = match password {
        Some(p) => sk_box.into_secret_key(Some(p.to_owned())),
        None => sk_box.into_unencrypted_secret_key(),
    }
    .map_err(|_| BAD_KEY.to_string())?;
    let trusted = format!("hyades {file_name}");
    minisign::sign(None, &sk, data, Some(&trusted), None)
        .map(|s| s.into_string())
        .map_err(|_| "signing failed".to_string())
}

/// `keygen --out <dir>`: an encrypted key pair; the secret key goes to
/// `<dir>/hyades-update.key` (outside the repository), the public key to
/// `packaging/update-key.pub`.
pub fn run_keygen(args: &[String]) -> Result<(), String> {
    let out = match args {
        [flag, dir] if flag == "--out" => PathBuf::from(dir),
        _ => return Err("usage: cargo xtask keygen --out <dir>".into()),
    };
    let out = expand_home(&out);
    let dir = checked_out_dir(&out)?;
    let first = rpassword::prompt_password("New key password: ").map_err(|e| e.to_string())?;
    let second = rpassword::prompt_password("Repeat password: ").map_err(|e| e.to_string())?;
    if first != second {
        return Err("the passwords differ".into());
    }
    if first.is_empty() {
        return Err("choose a non-empty password".into());
    }
    let public = crate::repo_root().join("packaging/update-key.pub");
    write_keys(&dir, &public, Some(first), PUBLIC_COMMENT)?;
    println!("secret key: {}", dir.join("hyades-update.key").display());
    println!("public key: {}", public.display());
    println!(
        "Back up the secret key file and the password: without them installed copies cannot verify updates."
    );
    Ok(())
}

fn expand_home(p: &Path) -> PathBuf {
    match (p.strip_prefix("~"), std::env::var_os("HOME")) {
        (Ok(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ => p.to_path_buf(),
    }
}

/// Creates `dir` and returns its real path; refuses anything inside the
/// repository (a secret key must never end up in a commit).
fn checked_out_dir(dir: &Path) -> Result<PathBuf, String> {
    let root = crate::repo_root();
    let root = root.canonicalize().unwrap_or(root);
    let inside = |p: &Path| p.starts_with(&root);
    let abs = std::path::absolute(dir).map_err(|e| e.to_string())?;
    let refuse = || "refusing to write the secret key inside the repository".to_string();
    if inside(&abs) {
        return Err(refuse());
    }
    fs::create_dir_all(&abs).map_err(|e| format!("{}: {e}", abs.display()))?;
    let real = abs.canonicalize().map_err(|e| e.to_string())?;
    if inside(&real) {
        return Err(refuse());
    }
    Ok(real)
}

/// Generates a key pair; `password` `None` makes an unencrypted key.
pub fn write_keys(
    dir: &Path,
    public: &Path,
    password: Option<String>,
    comment: &str,
) -> Result<(), String> {
    let pair = match password {
        Some(p) => KeyPair::generate_encrypted_keypair(Some(p)),
        None => KeyPair::generate_unencrypted_keypair(),
    }
    .map_err(|_| "key generation failed".to_string())?;
    let secret = pair
        .sk
        .to_box(Some("Hyades release key (secret)"))
        .map_err(|_| "key encoding failed".to_string())?
        .into_string();
    let key_file = dir.join("hyades-update.key");
    write_secret_file(&key_file, secret.as_bytes())?;
    let public_text = pair
        .pk
        .to_box()
        .map_err(|_| "key encoding failed".to_string())?
        .into_string();
    // Minisign writes its own untrusted comment; replace it.
    let base64 = public_text.lines().nth(1).unwrap_or_default();
    fs::write(public, format!("untrusted comment: {comment}\n{base64}\n"))
        .map_err(|e| format!("{}: {e}", public.display()))
}

fn write_secret_file(path: &Path, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write;
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .map_err(|e| format!("{}: {e} (it is never overwritten)", path.display()))?;
    file.write_all(bytes)
        .map_err(|e| format!("{}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_key() -> (String, String) {
        let pair = KeyPair::generate_unencrypted_keypair().unwrap();
        (
            pair.sk.to_box(None).unwrap().into_string(),
            pair.pk.to_base64(),
        )
    }

    #[test]
    fn sign_errors_never_quote_the_key_or_password() {
        let (secret, _) = test_key();
        let password = "hunter2-very-secret";
        // Unencrypted key with a password: fails, must not echo either.
        let err = sign_bytes(&secret, Some(password), b"data", "f").unwrap_err();
        assert!(!err.contains(password), "{err}");
        assert!(!err.contains(secret.lines().nth(1).unwrap()), "{err}");
        // Malformed key text.
        let junk = "untrusted comment: x\nSECRETJUNKVALUE";
        let err = sign_bytes(junk, Some(password), b"data", "f").unwrap_err();
        assert!(
            !err.contains("SECRETJUNKVALUE") && !err.contains(password),
            "{err}"
        );
    }

    #[test]
    fn keygen_writes_private_key_and_public_file() {
        let tmp = tempfile::tempdir().unwrap();
        let public = tmp.path().join("update-key.pub");
        write_keys(tmp.path(), &public, Some("pw".into()), "c").unwrap();
        let text = fs::read_to_string(&public).unwrap();
        assert!(text.starts_with("untrusted comment: c\n"));
        let secret = fs::read_to_string(tmp.path().join("hyades-update.key")).unwrap();
        let sig = sign_bytes(&secret, Some("pw"), b"x", "x").unwrap();
        assert!(sig.contains("hyades x"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(tmp.path().join("hyades-update.key"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        // Never overwritten.
        assert!(write_keys(tmp.path(), &public, Some("pw".into()), "c").is_err());
    }

    #[test]
    fn keygen_refuses_the_repository_tree() {
        let inside = crate::repo_root().join("packaging/never-created");
        assert!(checked_out_dir(&inside).is_err());
        assert!(!inside.exists());
    }
}
