//! "Add several links": the pasted text read line by line, each line
//! sorted by what it links to, and the downloads it makes. Pure: the
//! screen is in `grab_flow.rs`.

use std::collections::HashMap;
use std::path::Path;

use taurus_core::AddRequest;
use taurus_core::media::fast_path_host;
use taurus_core::torrent::source::{Source, classify};

/// What one line links to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineKind {
    /// A file on a web server.
    Http,
    /// A magnet link, a `.torrent` link or a local `.torrent` file.
    Torrent,
    /// A link to a well-known video site (read by yt-dlp).
    Media,
    /// A web page (the link ends in "/" or ".html"): its files are found
    /// with Grab links.
    Page,
    /// Not a link.
    Invalid,
}

impl LineKind {
    /// The name the screen uses for the kind.
    pub fn key(self) -> &'static str {
        match self {
            LineKind::Http => "http",
            LineKind::Torrent => "torrent",
            LineKind::Media => "media",
            LineKind::Page => "page",
            LineKind::Invalid => "invalid",
        }
    }
}

/// One line of the text that holds something.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BatchLine {
    /// The line's number in the text, from 1.
    pub n: usize,
    /// The line, trimmed.
    pub text: String,
    pub kind: LineKind,
    /// The `n` of the earlier line this one is another copy of (a
    /// mirror), when copies are treated as mirrors.
    pub mirror_of: Option<usize>,
}

/// What `text` links to, by the same rules as the engine: torrents by
/// `torrent::source::classify`, video sites by `media::fast_path_host`.
pub fn line_kind(text: &str) -> LineKind {
    let text = text.trim();
    match classify(text) {
        Ok(Source::Http(url)) if url.host_str().is_some_and(|h| !h.is_empty()) => {
            if fast_path_host(text) {
                LineKind::Media
            } else if url.path().ends_with('/') || url.path().ends_with(".html") {
                LineKind::Page
            } else {
                LineKind::Http
            }
        }
        Ok(Source::Http(_)) | Err(_) => LineKind::Invalid,
        Ok(_) => LineKind::Torrent,
    }
}

/// The last path segment of a web link, lower-cased ("" when there is
/// none).
fn file_key(text: &str) -> String {
    url::Url::parse(text)
        .ok()
        .and_then(|u| {
            u.path_segments()
                .and_then(|mut s| s.next_back().map(str::to_ascii_lowercase))
        })
        .unwrap_or_default()
}

/// The lines of `text` that hold something: blank lines and lines
/// starting with '#' are skipped. With `mirrors`, a web file line whose
/// file name (last path segment, any case) is the same as an earlier web
/// file line's is a mirror of that line.
pub fn parse_batch(text: &str, mirrors: bool) -> Vec<BatchLine> {
    let mut firsts: HashMap<String, usize> = HashMap::new();
    let mut lines = Vec::new();
    for (i, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let n = i + 1;
        let kind = line_kind(line);
        let mut mirror_of = None;
        if mirrors && kind == LineKind::Http {
            let key = file_key(line);
            if !key.is_empty() {
                mirror_of = firsts.get(&key).copied();
                firsts.entry(key).or_insert(n);
            }
        }
        lines.push(BatchLine {
            n,
            text: line.to_string(),
            kind,
            mirror_of,
        });
    }
    lines
}

/// Whether `line` becomes a download of its own.
pub fn is_added(line: &BatchLine) -> bool {
    line.mirror_of.is_none()
        && matches!(
            line.kind,
            LineKind::Http | LineKind::Torrent | LineKind::Media
        )
}

/// One download per added line (`is_added`), saved in `dest`: mirrors go
/// into their first line's `mirrors`; videos get the Simple defaults;
/// pages and anything else that is not a link are left out.
pub fn to_requests(lines: &[BatchLine], dest: &Path) -> Vec<AddRequest> {
    lines
        .iter()
        .filter(|l| is_added(l))
        .map(|l| {
            let mut mirrors: Vec<String> = Vec::new();
            for m in lines.iter().filter(|m| m.mirror_of == Some(l.n)) {
                if m.text != l.text && !mirrors.contains(&m.text) {
                    mirrors.push(m.text.clone());
                }
            }
            AddRequest {
                url: l.text.clone(),
                dest_dir: dest.to_path_buf(),
                mirrors,
                ..Default::default()
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAGNET: &str = "magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567";

    fn kinds(lines: &[BatchLine]) -> Vec<(usize, LineKind)> {
        lines.iter().map(|l| (l.n, l.kind)).collect()
    }

    #[test]
    fn batch_kinds() {
        let text = [
            "https://example.org/a.iso",
            MAGNET,
            "https://example.org/linux.torrent",
            "https://www.youtube.com/watch?v=abc",
            "https://youtu.be/abc",
            "https://example.org/files/",
            "https://example.org/index.html",
            "hello there",
            "ftp://example.org/a.iso",
            "magnet:?xt=nothing",
            "  http://mirror.example.net/b.zip  ",
        ]
        .join("\n");
        let lines = parse_batch(&text, false);
        assert_eq!(
            kinds(&lines),
            vec![
                (1, LineKind::Http),
                (2, LineKind::Torrent),
                (3, LineKind::Torrent),
                (4, LineKind::Media),
                (5, LineKind::Media),
                (6, LineKind::Page),
                (7, LineKind::Page),
                (8, LineKind::Invalid),
                (9, LineKind::Invalid),
                (10, LineKind::Invalid),
                (11, LineKind::Http),
            ]
        );
        assert_eq!(lines[10].text, "http://mirror.example.net/b.zip");
        assert!(lines.iter().all(|l| l.mirror_of.is_none()));
        let dest = Path::new("/data/dl");
        let urls: Vec<String> = to_requests(&lines, dest)
            .into_iter()
            .map(|r| {
                assert_eq!(r.dest_dir, dest);
                assert!(r.media.is_none() && r.mirrors.is_empty());
                r.url
            })
            .collect();
        assert_eq!(
            urls,
            vec![
                "https://example.org/a.iso",
                MAGNET,
                "https://example.org/linux.torrent",
                "https://www.youtube.com/watch?v=abc",
                "https://youtu.be/abc",
                "http://mirror.example.net/b.zip",
            ]
        );
    }

    #[test]
    fn batch_mirrors_fold() {
        let text = "https://a.example.org/pub/Linux.ISO\n\
                    https://b.example.net/mirror/linux.iso?x=1\n\
                    https://c.example.com/other.iso\n\
                    https://d.example.com/dl/LINUX.iso\n\
                    https://a.example.org/pub/Linux.ISO\n\
                    https://www.youtube.com/linux.iso\n";
        let lines = parse_batch(text, true);
        let mirror_of: Vec<Option<usize>> = lines.iter().map(|l| l.mirror_of).collect();
        assert_eq!(mirror_of, vec![None, Some(1), None, Some(1), Some(1), None]);
        let requests = to_requests(&lines, Path::new("/dl"));
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[0].url, "https://a.example.org/pub/Linux.ISO");
        assert_eq!(
            requests[0].mirrors,
            vec![
                "https://b.example.net/mirror/linux.iso?x=1".to_string(),
                "https://d.example.com/dl/LINUX.iso".to_string(),
            ]
        );
        assert_eq!(requests[1].url, "https://c.example.com/other.iso");
        assert!(requests[1].mirrors.is_empty());
        assert_eq!(requests[2].url, "https://www.youtube.com/linux.iso");

        // Without the switch every copy is a download of its own.
        let plain = parse_batch(text, false);
        assert!(plain.iter().all(|l| l.mirror_of.is_none()));
        assert_eq!(to_requests(&plain, Path::new("/dl")).len(), 6);
    }

    #[test]
    fn batch_skips_comments() {
        let text = "# my links\n\n   \n  # indented comment\nhttps://example.org/a.bin\r\n\t\nhttps://example.org/b.bin # not a comment\n";
        let lines = parse_batch(text, true);
        assert_eq!(
            lines
                .iter()
                .map(|l| (l.n, l.text.as_str()))
                .collect::<Vec<_>>(),
            vec![
                (5, "https://example.org/a.bin"),
                (7, "https://example.org/b.bin # not a comment"),
            ]
        );
        assert!(parse_batch("", false).is_empty());
        assert!(parse_batch("#only\n\n", false).is_empty());
    }
}
