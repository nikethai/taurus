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
