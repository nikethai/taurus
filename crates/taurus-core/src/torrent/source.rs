//! Telling magnet links, `.torrent` URLs, local `.torrent` files and plain
//! HTTP links apart.

use std::path::PathBuf;

use url::Url;

use crate::errors::{CoreError, ErrorKind};

#[derive(Clone, Debug, PartialEq)]
pub enum Source {
    Http(Url),
    Magnet {
        uri: String,
        /// 40 lowercase hex characters.
        info_hash: String,
        name: Option<String>,
    },
    TorrentUrl(Url),
    TorrentFile(PathBuf),
}

fn error(kind: ErrorKind, detail: impl Into<String>) -> CoreError {
    CoreError {
        kind,
        detail: detail.into(),
    }
}

fn has_torrent_ext(s: &str) -> bool {
    s.len() >= 8
        && s.is_char_boundary(s.len() - 8)
        && s[s.len() - 8..].eq_ignore_ascii_case(".torrent")
}

/// Classifies what the user typed or dropped.
pub fn classify(input: &str) -> Result<Source, CoreError> {
    let input = input.trim();
    if input
        .get(..7)
        .is_some_and(|p| p.eq_ignore_ascii_case("magnet:"))
    {
        return magnet(input);
    }
    let parsed = Url::parse(input);
    if let Ok(url) = &parsed
        && matches!(url.scheme(), "http" | "https")
    {
        return Ok(if has_torrent_ext(url.path()) {
            Source::TorrentUrl(url.clone())
        } else {
            Source::Http(url.clone())
        });
    }
    // Checked after URLs, but a Windows path (`C:\x.torrent`) parses as a URL
    // with a one-letter scheme, so it must not be rejected above.
    let path = PathBuf::from(input);
    if has_torrent_ext(input) && path.is_file() {
        return Ok(Source::TorrentFile(path));
    }
    match parsed {
        Ok(_) => Err(error(
            ErrorKind::Unknown,
            format!("invalid URL {input:?}: unsupported scheme"),
        )),
        Err(e) => Err(error(
            ErrorKind::Unknown,
            format!("invalid URL {input:?}: {e}"),
        )),
    }
}

fn magnet(input: &str) -> Result<Source, CoreError> {
    let invalid = || error(ErrorKind::SiteNotSupported, "not a valid magnet link");
    let parsed = librqbit::Magnet::parse(input).map_err(|_| invalid())?;
    // A v2-only magnet has no v1 hash to download by.
    let id = parsed.as_id20().ok_or_else(invalid)?;
    Ok(Source::Magnet {
        uri: input.to_string(),
        info_hash: id.as_string(),
        name: parsed.name.filter(|n| !n.is_empty()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const HASH: &str = "c12fe1c06bba254a9dc9f519b335aa7c1367a88a";

    fn magnet_parts(input: &str) -> (String, String, Option<String>) {
        match classify(input).unwrap() {
            Source::Magnet {
                uri,
                info_hash,
                name,
            } => (uri, info_hash, name),
            other => panic!("not a magnet: {other:?}"),
        }
    }

    #[test]
    fn magnet_hex_info_hash() {
        let upper = HASH.to_uppercase();
        let (uri, hash, name) = magnet_parts(&format!("magnet:?xt=urn:btih:{upper}"));
        assert_eq!(hash, HASH);
        assert_eq!(uri, format!("magnet:?xt=urn:btih:{upper}"));
        assert_eq!(name, None);
        // Surrounding whitespace and a mixed-case scheme are fine.
        let (_, hash, _) = magnet_parts(&format!("  MAGNET:?xt=urn:btih:{HASH}\n"));
        assert_eq!(hash, HASH);
    }

    #[test]
    fn magnet_base32_info_hash() {
        // base32 of the same 20 bytes.
        let bytes = hex::decode(HASH).unwrap();
        let b32 = base32(&bytes);
        let (_, hash, _) = magnet_parts(&format!("magnet:?xt=urn:btih:{b32}"));
        assert_eq!(hash, HASH);
    }

    fn base32(bytes: &[u8]) -> String {
        const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
        let (mut out, mut buf, mut bits) = (String::new(), 0u32, 0u32);
        for &b in bytes {
            buf = (buf << 8) | u32::from(b);
            bits += 8;
            while bits >= 5 {
                out.push(ALPHABET[((buf >> (bits - 5)) & 31) as usize] as char);
                bits -= 5;
            }
        }
        out
    }

    #[test]
    fn magnet_name_decoded() {
        let (_, _, name) = magnet_parts(&format!("magnet:?xt=urn:btih:{HASH}&dn=My%20Show"));
        assert_eq!(name.as_deref(), Some("My Show"));
    }

    #[test]
    fn bad_magnet_rejected() {
        for input in [
            "magnet:?dn=nothing",
            "magnet:?xt=urn:btih:zzzz",
            "magnet:",
            // v2-only: no v1 hash to download by.
            "magnet:?xt=urn:btmh:1220c12fe1c06bba254a9dc9f519b335aa7c1367a88ac12fe1c06bba254a9dc9",
        ] {
            let e = classify(input).unwrap_err();
            assert_eq!(e.kind, ErrorKind::SiteNotSupported, "{input}");
            assert_eq!(e.detail, "not a valid magnet link", "{input}");
        }
    }

    #[test]
    fn torrent_url_vs_http_url() {
        assert!(matches!(
            classify("https://x.org/dl/a.torrent").unwrap(),
            Source::TorrentUrl(_)
        ));
        assert!(matches!(
            classify("http://x.org/dl/A.TORRENT?token=1").unwrap(),
            Source::TorrentUrl(_)
        ));
        // The query does not count.
        assert!(matches!(
            classify("https://x.org/get?file=a.torrent").unwrap(),
            Source::Http(_)
        ));
        assert!(matches!(
            classify("https://x.org/a.iso").unwrap(),
            Source::Http(_)
        ));
    }

    #[test]
    fn local_torrent_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.torrent");
        std::fs::write(&path, b"d4:infod4:name1:aee").unwrap();
        let input = path.to_str().unwrap();
        assert_eq!(classify(input).unwrap(), Source::TorrentFile(path.clone()));
        // A missing file is an invalid URL, as before.
        let missing = dir.path().join("nope.torrent");
        let e = classify(missing.to_str().unwrap()).unwrap_err();
        assert_eq!(e.kind, ErrorKind::Unknown);
        assert!(e.detail.starts_with("invalid URL"), "{}", e.detail);
    }

    #[test]
    fn garbage_is_invalid_url() {
        let e = classify("not a url").unwrap_err();
        assert_eq!(e.kind, ErrorKind::Unknown);
        assert!(e.detail.starts_with("invalid URL"), "{}", e.detail);
        assert_eq!(
            classify("ftp://x.org/a").unwrap_err().kind,
            ErrorKind::Unknown
        );
    }
}
