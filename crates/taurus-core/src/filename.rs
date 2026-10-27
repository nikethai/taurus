//! Download file names: extraction, sanitizing and collision avoidance.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use percent_encoding::percent_decode_str;
use url::Url;

/// Extension of in-progress files (`<final name>.tapa`).
pub const PARTIAL_EXT: &str = "tapa";

const FALLBACK_NAME: &str = "download";
const MAX_NAME_BYTES: usize = 255;
const MAX_EXT_BYTES: usize = 16;
const RESERVED_STEMS: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// Extracts the raw (unsanitized) file name from a `Content-Disposition`
/// header value. `filename*` (RFC 5987, UTF-8) wins over `filename`.
pub fn from_content_disposition(header: &str) -> Option<String> {
    let mut plain = None;
    let mut extended = None;
    for part in split_params(header) {
        let Some((key, value)) = part.split_once('=') else {
            continue;
        };
        let value = parse_value(value);
        match key.trim().to_ascii_lowercase().as_str() {
            "filename" => plain = plain.or(Some(value)),
            "filename*" => extended = extended.or_else(|| decode_ext_value(&value)),
            _ => {}
        }
    }
    extended
        .into_iter()
        .chain(plain)
        .find(|name| !name.is_empty())
}

/// Splits on `;` outside of quoted strings.
fn split_params(header: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let (mut start, mut in_quotes, mut escaped) = (0, false, false);
    for (i, c) in header.char_indices() {
        match c {
            _ if escaped => escaped = false,
            '\\' if in_quotes => escaped = true,
            '"' => in_quotes = !in_quotes,
            ';' if !in_quotes => {
                parts.push(&header[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(&header[start..]);
    parts
}

/// Unquotes a parameter value (handling `\x` escapes) or trims a bare token.
fn parse_value(raw: &str) -> String {
    let raw = raw.trim();
    let Some(quoted) = raw.strip_prefix('"') else {
        return raw.to_string();
    };
    let mut out = String::new();
    let mut chars = quoted.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => out.extend(chars.next()),
            '"' => break,
            _ => out.push(c),
        }
    }
    out
}

/// Decodes `UTF-8'lang'percent-encoded`; other charsets are unsupported.
fn decode_ext_value(value: &str) -> Option<String> {
    let mut it = value.splitn(3, '\'');
    let charset = it.next()?;
    let _lang = it.next()?;
    let encoded = it.next()?;
    charset
        .eq_ignore_ascii_case("utf-8")
        .then(|| percent_decode_str(encoded).decode_utf8_lossy().into_owned())
}

/// Last non-empty path segment of the URL, percent-decoded.
pub fn from_url(url: &Url) -> Option<String> {
    let segment = url.path_segments()?.rfind(|s| !s.is_empty())?;
    let decoded = percent_decode_str(segment).decode_utf8_lossy().into_owned();
    (!decoded.is_empty()).then_some(decoded)
}

/// Common file extension (without the dot) for a `Content-Type` value.
pub fn extension_for_content_type(ct: &str) -> Option<&'static str> {
    let essence = ct
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    Some(match essence.as_str() {
        "application/zip" => "zip",
        "application/pdf" => "pdf",
        "application/x-bittorrent" => "torrent",
        "application/gzip" => "gz",
        "application/x-7z-compressed" => "7z",
        "application/vnd.rar" | "application/x-rar-compressed" => "rar",
        "application/x-iso9660-image" => "iso",
        "video/mp4" => "mp4",
        "video/webm" => "webm",
        "audio/mpeg" => "mp3",
        "image/jpeg" => "jpg",
        "image/png" => "png",
        "text/plain" => "txt",
        "text/html" => "html",
        "application/json" => "json",
        _ => return None,
    })
}

/// Picks a safe file name: `Content-Disposition`, else the URL, else
/// `download` (which alone gets an extension derived from the content type).
pub fn choose_name(
    disposition: Option<&str>,
    final_url: &Url,
    content_type: Option<&str>,
) -> String {
    if let Some(name) = disposition
        .and_then(from_content_disposition)
        .or_else(|| from_url(final_url))
    {
        return sanitize(&name);
    }
    match content_type.and_then(extension_for_content_type) {
        Some(ext) => format!("{FALLBACK_NAME}.{ext}"),
        None => FALLBACK_NAME.to_string(),
    }
}

/// Makes `name` safe as a single path component on every platform.
pub fn sanitize(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or("");
    let replaced: String = base
        .chars()
        .map(|c| {
            if matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*') || c.is_ascii_control() {
                '_'
            } else {
                c
            }
        })
        .collect();
    let trimmed = replaced
        .trim_start()
        .trim_end_matches(|c: char| c == '.' || c.is_whitespace());
    if trimmed.is_empty() {
        return FALLBACK_NAME.to_string();
    }

    let stem = trimmed.split('.').next().unwrap_or("");
    let named = if RESERVED_STEMS.iter().any(|r| r.eq_ignore_ascii_case(stem)) {
        format!("_{trimmed}")
    } else {
        trimmed.to_string()
    };
    truncate_name(named)
}

/// Caps the name at 255 bytes on a char boundary, keeping a short extension.
fn truncate_name(name: String) -> String {
    if name.len() <= MAX_NAME_BYTES {
        return name;
    }
    let candidate = fit_name(&name, "", MAX_NAME_BYTES);
    let trimmed = candidate.trim_end_matches(|c: char| c == '.' || c.is_whitespace());
    if trimmed.is_empty() {
        FALLBACK_NAME.to_string()
    } else {
        trimmed.to_string()
    }
}

/// Splits off the extension (from the last `.`), unless the name starts with
/// that dot or the extension is longer than `MAX_EXT_BYTES`.
fn split_ext(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(i) if i > 0 && name.len() - i <= MAX_EXT_BYTES => name.split_at(i),
        _ => (name, ""),
    }
}

/// `stem + suffix + ext` of `name`, with the stem truncated on a char
/// boundary so the whole is at most `max` bytes.
fn fit_name(name: &str, suffix: &str, max: usize) -> String {
    let (stem, ext) = split_ext(name);
    let budget = max.saturating_sub(suffix.len() + ext.len());
    format!("{}{suffix}{ext}", &stem[..floor_boundary(stem, budget)])
}

/// Largest char boundary of `s` that is `<= max`.
fn floor_boundary(s: &str, max: usize) -> usize {
    (0..=max.min(s.len()))
        .rev()
        .find(|&i| s.is_char_boundary(i))
        .unwrap_or(0)
}

/// First free `dir/<name>` (then `name (1).ext`, `name (2).ext`, ...). A
/// candidate is taken if the file or its `.tapa` partial exists, or it is in
/// `reserved`. Every candidate is cut (stem only, extension kept) so that
/// `<candidate>.tapa` stays within 255 bytes.
pub fn unique_path(dir: &Path, name: &str, reserved: &HashSet<String>) -> PathBuf {
    let taken = |candidate: &str| {
        reserved.contains(candidate)
            || dir.join(candidate).symlink_metadata().is_ok()
            || dir
                .join(format!("{candidate}.{PARTIAL_EXT}"))
                .symlink_metadata()
                .is_ok()
    };
    let max = MAX_NAME_BYTES - (PARTIAL_EXT.len() + 1);
    std::iter::once(fit_name(name, "", max))
        .chain((1..).map(|n| fit_name(name, &format!(" ({n})"), max)))
        .find(|candidate| !taken(candidate))
        .map(|candidate| dir.join(candidate))
        .expect("unbounded counter yields a free name")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn disposition_plain() {
        assert_eq!(
            from_content_disposition(r#"attachment; filename="report.pdf""#).as_deref(),
            Some("report.pdf")
        );
    }

    #[test]
    fn disposition_rfc5987_wins() {
        let h = "attachment; filename=\"a.txt\"; filename*=UTF-8''%C3%A1nh%20%C4%91%E1%BA%B9p.jpg";
        assert_eq!(from_content_disposition(h).as_deref(), Some("ánh đẹp.jpg"));
        let h = "attachment; filename*=utf-8'vi'%C3%A1.jpg; filename=\"a.txt\"";
        assert_eq!(from_content_disposition(h).as_deref(), Some("á.jpg"));
    }

    #[test]
    fn disposition_variants() {
        assert_eq!(
            from_content_disposition("attachment; filename=x.bin").as_deref(),
            Some("x.bin")
        );
        assert_eq!(
            from_content_disposition(r#"attachment; FileName="Up.TXT""#).as_deref(),
            Some("Up.TXT")
        );
        assert_eq!(
            from_content_disposition(r#"attachment; filename="a\"b;c.txt""#).as_deref(),
            Some(r#"a"b;c.txt"#)
        );
        // Unsupported charset in filename* falls back to filename.
        assert_eq!(
            from_content_disposition(
                "attachment; filename*=ISO-8859-1''%E1.txt; filename=\"f.txt\""
            )
            .as_deref(),
            Some("f.txt")
        );
        // Invalid UTF-8 after decoding is decoded lossily.
        assert_eq!(
            from_content_disposition("attachment; filename*=UTF-8''a%FFb").as_deref(),
            Some("a\u{FFFD}b")
        );
        assert_eq!(from_content_disposition("inline"), None);
        assert_eq!(from_content_disposition(r#"attachment; filename="""#), None);
    }

    #[test]
    fn url_percent_decoded() {
        assert_eq!(
            from_url(&url("https://x.org/dl/My%20File.iso?x=1")).as_deref(),
            Some("My File.iso")
        );
        assert_eq!(from_url(&url("https://x.org/")), None);
        assert_eq!(from_url(&url("https://x.org/a/b/")).as_deref(), Some("b"));
    }

    #[test]
    fn content_type_extensions() {
        assert_eq!(extension_for_content_type("application/zip"), Some("zip"));
        assert_eq!(
            extension_for_content_type("Text/HTML; charset=utf-8"),
            Some("html")
        );
        assert_eq!(extension_for_content_type("application/octet-stream"), None);
        assert_eq!(extension_for_content_type("application/x-unknown"), None);
    }

    #[test]
    fn choose_name_fallback() {
        let root = url("https://x.org/");
        assert_eq!(
            choose_name(None, &root, Some("application/zip")),
            "download.zip"
        );
        assert_eq!(choose_name(None, &root, None), "download");
        assert_eq!(
            choose_name(None, &root, Some("application/octet-stream")),
            "download"
        );
    }

    #[test]
    fn choose_name_precedence() {
        let u = url("https://x.org/dl/file.iso");
        assert_eq!(
            choose_name(
                Some("attachment; filename=\"../r.pdf\""),
                &u,
                Some("application/zip")
            ),
            "r.pdf"
        );
        // No generated extension when the name came from the URL.
        assert_eq!(
            choose_name(None, &url("https://x.org/dl/file"), Some("application/zip")),
            "file"
        );
        assert_eq!(choose_name(Some("inline"), &u, None), "file.iso");
    }

    #[test]
    fn sanitize_cases() {
        assert_eq!(sanitize("../../etc/passwd"), "passwd");
        assert_eq!(sanitize(r"C:\dir\evil.exe"), "evil.exe");
        assert_eq!(sanitize("a<b>c:d\"e|f?g*h.txt"), "a_b_c_d_e_f_g_h.txt");
        assert_eq!(sanitize("a\u{1}b\tc"), "a_b_c");
        assert_eq!(sanitize("CON.txt"), "_CON.txt");
        assert_eq!(sanitize("lpt9"), "_lpt9");
        assert_eq!(sanitize("console.txt"), "console.txt");
        assert_eq!(sanitize("name. . "), "name");
        assert_eq!(sanitize("  spaced.txt  "), "spaced.txt");
        assert_eq!(sanitize(".bashrc"), ".bashrc");
        assert_eq!(sanitize(""), "download");
        assert_eq!(sanitize("."), "download");
        assert_eq!(sanitize(".."), "download");
        assert_eq!(sanitize("dir/"), "download");
    }

    #[test]
    fn sanitize_truncates_long_names() {
        let name = format!("{}.iso", "é".repeat(150));
        assert_eq!(name.len(), 304);
        let out = sanitize(&name);
        assert!(out.len() <= 255, "len {}", out.len());
        assert!(out.ends_with(".iso"));
        assert!(out.starts_with('é'));

        let no_ext = "ß".repeat(200);
        let out = sanitize(&no_ext);
        assert!(out.len() <= 255);
        assert!(out.chars().all(|c| c == 'ß'));

        // A long "extension" is not preserved.
        let long_ext = format!("a.{}", "x".repeat(300));
        assert_eq!(sanitize(&long_ext).len(), 255);
    }

    #[test]
    fn unique_path_suffixes() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        let none = HashSet::new();

        assert_eq!(unique_path(d, "a.iso", &none), d.join("a.iso"));

        fs::write(d.join("a.iso"), b"").unwrap();
        assert_eq!(unique_path(d, "a.iso", &none), d.join("a (1).iso"));

        fs::write(d.join("a (1).iso.tapa"), b"").unwrap();
        assert_eq!(unique_path(d, "a.iso", &none), d.join("a (2).iso"));

        let reserved: HashSet<String> = ["a (2).iso".to_string()].into();
        assert_eq!(unique_path(d, "a.iso", &reserved), d.join("a (3).iso"));

        fs::write(d.join("README"), b"").unwrap();
        assert_eq!(unique_path(d, "README", &none), d.join("README (1)"));

        fs::write(d.join(".bashrc"), b"").unwrap();
        assert_eq!(unique_path(d, ".bashrc", &none), d.join(".bashrc (1)"));

        fs::write(d.join("b.tar.gz.tapa"), b"").unwrap();
        assert_eq!(unique_path(d, "b.tar.gz", &none), d.join("b.tar (1).gz"));
    }

    #[test]
    fn unique_path_keeps_partial_name_within_255_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        let none = HashSet::new();
        let max = 255 - ".tapa".len();

        let name = format!("{}a.iso", "é".repeat(125));
        assert_eq!(name.len(), 255);
        let name = sanitize(&name);
        assert!(name.len() > max);

        let first = unique_path(d, &name, &none);
        let first_name = first.file_name().unwrap().to_str().unwrap().to_string();
        assert!(
            first_name.len() + ".tapa".len() <= 255,
            "{}",
            first_name.len()
        );
        assert!(first_name.ends_with(".iso"));
        assert!(first_name.starts_with('é'));

        fs::write(&first, b"").unwrap();
        let second = unique_path(d, &name, &none);
        let second_name = second.file_name().unwrap().to_str().unwrap();
        assert_ne!(second_name, first_name);
        assert!(second_name.len() <= max, "{}", second_name.len());
        assert!(second_name.contains(" (1)"));
        assert!(second_name.ends_with(".iso"));
        assert!(second_name.starts_with('é'));
    }
}
