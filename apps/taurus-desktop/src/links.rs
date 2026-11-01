//! Spotting a download link in pasted or copied text.

const MAX_LEN: usize = 4096;

/// The trimmed text when it is exactly one http(s) or magnet link: at most
/// 4096 bytes, nothing but the link (no whitespace or control characters).
pub fn candidate(text: &str) -> Option<String> {
    let t = text.trim();
    if t.is_empty() || t.len() > MAX_LEN || t.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return None;
    }
    let lower = t.to_ascii_lowercase();
    let rest = lower
        .strip_prefix("https://")
        .or_else(|| lower.strip_prefix("http://"))
        .or_else(|| lower.strip_prefix("magnet:?"))?;
    if rest.is_empty() {
        return None;
    }
    Some(t.to_string())
}

/// The downloads named on the command line: each argument that is a link
/// (`candidate`) or an existing `.torrent` file, the file as an absolute
/// path (a running instance may have another working folder). Flags and
/// anything else, non-UTF-8 arguments included, are left out.
pub fn launch_links(args: impl IntoIterator<Item = std::ffi::OsString>) -> Vec<String> {
    args.into_iter()
        .filter_map(|arg| {
            let text = arg.to_str()?;
            if let Some(link) = candidate(text) {
                return Some(link);
            }
            let path = std::path::Path::new(text);
            let is_torrent = path
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("torrent"));
            if !is_torrent || !path.is_file() {
                return None;
            }
            std::path::absolute(path).ok()?.to_str().map(str::to_string)
        })
        .collect()
}

/// Offers each new link on the clipboard once, and never one the app put
/// there itself.
#[derive(Debug, Default)]
pub struct ClipboardFilter {
    last_seen: Option<String>,
    last_copied: Option<String>,
}

impl ClipboardFilter {
    pub fn new() -> Self {
        Self::default()
    }

    /// The app wrote `text` to the clipboard.
    pub fn we_copied(&mut self, text: &str) {
        self.last_copied = Some(text.trim().to_string());
        self.last_seen = Some(text.to_string());
    }

    /// The clipboard now holds `text`. `Some(link)` only for a link that is
    /// new since the last call and was not copied by us.
    pub fn offer(&mut self, text: &str) -> Option<String> {
        if self.last_seen.as_deref() == Some(text) {
            return None;
        }
        self.last_seen = Some(text.to_string());
        if self.last_copied.as_deref() != Some(text.trim()) {
            self.last_copied = None;
        }
        let link = candidate(text)?;
        if self.last_copied.as_deref() == Some(link.as_str()) {
            return None;
        }
        Some(link)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clipboard_candidates() {
        assert_eq!(
            candidate("https://x.org/a.iso").as_deref(),
            Some("https://x.org/a.iso")
        );
        assert_eq!(
            candidate("  magnet:?xt=urn:btih:abcdef0123456789  \n").as_deref(),
            Some("magnet:?xt=urn:btih:abcdef0123456789")
        );
        assert_eq!(candidate("HTTP://x.org").as_deref(), Some("HTTP://x.org"));
        assert_eq!(candidate("hello"), None);
        assert_eq!(candidate(""), None);
        assert_eq!(candidate("https://"), None);
        assert_eq!(candidate("https://a.org/x https://b.org/y"), None);
        assert_eq!(candidate("https://a.org/x\nhttps://b.org/y"), None);
        assert_eq!(
            candidate(&format!("https://x.org/{}", "a".repeat(5000))),
            None
        );
        assert_eq!(candidate("ftp://x"), None);
        let at_limit = format!("https://x.org/{}", "a".repeat(MAX_LEN - 14));
        assert_eq!(at_limit.len(), MAX_LEN);
        assert!(candidate(&at_limit).is_some());
    }

    #[test]
    fn launch_links_keep_links_and_torrent_files() {
        let dir = tempfile::tempdir().unwrap();
        let torrent = dir.path().join("linux.TORRENT");
        std::fs::write(&torrent, b"d4:infode").unwrap();
        let other = dir.path().join("notes.txt");
        std::fs::write(&other, b"hi").unwrap();
        let args = [
            "--help".into(),
            "-v".into(),
            "https://x.org/a.iso".into(),
            torrent.clone().into_os_string(),
            dir.path().join("missing.torrent").into_os_string(),
            other.into_os_string(),
            "hello".into(),
            "magnet:?xt=urn:btih:abc".into(),
        ];
        assert_eq!(
            launch_links(args),
            vec![
                "https://x.org/a.iso".to_string(),
                torrent.to_str().unwrap().to_string(),
                "magnet:?xt=urn:btih:abc".to_string(),
            ]
        );
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            let bad = std::ffi::OsString::from_vec(vec![b'h', 0xff, b'.', b't']);
            assert!(launch_links([bad]).is_empty());
        }
    }

    #[test]
    fn clipboard_filter() {
        let mut f = ClipboardFilter::new();
        assert_eq!(
            f.offer("https://x.org/a.iso").as_deref(),
            Some("https://x.org/a.iso")
        );
        assert_eq!(f.offer("https://x.org/a.iso"), None);
        assert_eq!(f.offer("hello"), None);
        // copied again by the user after other text: offered again
        assert!(f.offer("https://x.org/a.iso").is_some());

        let mut f = ClipboardFilter::new();
        f.we_copied("https://x.org/b.iso");
        assert_eq!(f.offer("https://x.org/b.iso"), None);
        assert!(f.offer("https://x.org/c.iso").is_some());
        // we copied b earlier, but the user copied c since: b is theirs now
        assert!(f.offer("https://x.org/b.iso").is_some());
    }
}
