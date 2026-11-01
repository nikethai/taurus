//! SHA-256 checksums: a sum the user gives, one published in a `SHA256SUMS`
//! file next to the download, and the sum of a file on disk.

use std::io::Read;
use std::path::Path;

use sha2::{Digest, Sha256};

/// `hash_file` reads this much at a time.
const HASH_BUFFER: usize = 1024 * 1024;
/// The file a host publishes its sums in, beside the files.
const SUMS_FILE: &str = "SHA256SUMS";

/// A SHA-256 sum as 64 lowercase hex digits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sha256Sum(pub String);

/// Reads `sha256:<hex>` or a bare 64-digit hex sum, in any case.
pub fn parse_sum(s: &str) -> Option<Sha256Sum> {
    let s = s.trim();
    let hex = match s.get(..7) {
        Some(prefix) if prefix.eq_ignore_ascii_case("sha256:") => &s[7..],
        _ => s,
    };
    (hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()))
        .then(|| Sha256Sum(hex.to_ascii_lowercase()))
}

/// The sum of `file_name` in the text of a `SHA256SUMS` file, whose lines
/// read `<hex>  name` (text mode) or `<hex> *name` (binary mode).
pub fn find_in_sums_file(text: &str, file_name: &str) -> Option<Sha256Sum> {
    text.lines().find_map(|line| {
        let line = line.trim_end();
        let (hex, rest) = (line.get(..64)?, line.get(64..)?);
        let name = rest.strip_prefix(' ')?;
        let name = name
            .strip_prefix('*')
            .or_else(|| name.strip_prefix(' '))
            .unwrap_or(name);
        if name == file_name {
            parse_sum(hex)
        } else {
            None
        }
    })
}

/// `SHA256SUMS` in the same directory as `final_url`, for http(s) URLs.
pub fn sums_url(final_url: &url::Url) -> Option<url::Url> {
    if !matches!(final_url.scheme(), "http" | "https") {
        return None;
    }
    final_url.join(SUMS_FILE).ok()
}

/// The SHA-256 of the file at `path`, read on a blocking thread.
pub async fn hash_file(path: &Path) -> std::io::Result<Sha256Sum> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let mut file = std::fs::File::open(path)?;
        let mut hasher = Sha256::new();
        let mut buf = vec![0; HASH_BUFFER];
        loop {
            let n = file.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
        Ok(Sha256Sum(hex::encode(hasher.finalize())))
    })
    .await
    .unwrap_or_else(|e| Err(std::io::Error::other(e)))
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEX: &str = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08";

    #[test]
    fn parse_sum_forms() {
        let want = Some(Sha256Sum(HEX.to_string()));
        assert_eq!(parse_sum(HEX), want);
        assert_eq!(parse_sum(&HEX.to_ascii_uppercase()), want);
        assert_eq!(parse_sum(&format!("sha256:{HEX}")), want);
        assert_eq!(
            parse_sum(&format!("SHA256:{}", HEX.to_ascii_uppercase())),
            want
        );
        assert_eq!(parse_sum(&format!("  {HEX}\n")), want);
        assert_eq!(parse_sum(&HEX[..63]), None);
        assert_eq!(parse_sum(&format!("{HEX}0")), None);
        assert_eq!(parse_sum(&format!("md5:{HEX}")), None);
        assert_eq!(parse_sum(&HEX.replace('9', "g")), None);
        assert_eq!(parse_sum(""), None);
    }

    #[test]
    fn find_in_sums_file_both_formats() {
        let other = "a".repeat(64);
        let text = format!(
            "{other}  other.iso\r\n{HEX}  x.iso\r\n{}  *y.bin\n{other} *z.bin\n",
            HEX.to_ascii_uppercase()
        );
        let want = Some(Sha256Sum(HEX.to_string()));
        assert_eq!(find_in_sums_file(&text, "x.iso"), want);
        assert_eq!(find_in_sums_file(&text, "y.bin"), None, "two spaces then *");
        assert_eq!(
            find_in_sums_file(&format!("{HEX} *y.bin\n"), "y.bin"),
            want,
            "binary marker"
        );
        assert_eq!(
            find_in_sums_file(&text, "z.bin"),
            Some(Sha256Sum(other.clone()))
        );
        assert_eq!(find_in_sums_file(&text, "missing.iso"), None);
        assert_eq!(find_in_sums_file(&text, "x"), None);
        assert_eq!(find_in_sums_file("", "x.iso"), None);
    }

    #[test]
    fn sums_url_same_dir() {
        let url = url::Url::parse("https://h/a/b/x.iso?t=1").unwrap();
        assert_eq!(sums_url(&url).unwrap().as_str(), "https://h/a/b/SHA256SUMS");
        let root = url::Url::parse("http://h:8080/x.iso#frag").unwrap();
        assert_eq!(
            sums_url(&root).unwrap().as_str(),
            "http://h:8080/SHA256SUMS"
        );
        assert_eq!(
            sums_url(&url::Url::parse("data:text/plain,x").unwrap()),
            None
        );
        assert_eq!(sums_url(&url::Url::parse("ftp://h/a/x.iso").unwrap()), None);
    }

    #[tokio::test]
    async fn hash_file_matches_sha256() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f");
        // Spans more than one buffer.
        let data: Vec<u8> = (0..HASH_BUFFER * 2 + 7).map(|i| i as u8).collect();
        std::fs::write(&path, &data).unwrap();
        assert_eq!(
            hash_file(&path).await.unwrap(),
            Sha256Sum(hex::encode(Sha256::digest(&data)))
        );
        assert!(hash_file(&dir.path().join("missing")).await.is_err());
    }
}
