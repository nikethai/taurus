//! The operating system's "downloaded from the internet" mark, which makes
//! the desktop warn before a downloaded program runs: Windows'
//! `Zone.Identifier` stream, macOS' quarantine attribute, and the xdg origin
//! attributes on Linux. Best-effort: a file system without the stream or
//! attributes leaves the file unmarked.

use std::path::Path;

/// Marks the file at `path` as downloaded from `url`, linked from `referer`.
/// User names and passwords are left out of the URLs (the query stays); a
/// URL that does not parse is left out. A failure is logged, never returned.
pub fn mark_downloaded(path: &Path, url: &str, referer: Option<&str>) {
    let url = without_credentials(url);
    let referer = referer.and_then(without_credentials);
    if let Err(e) = write_mark(path, url.as_deref(), referer.as_deref()) {
        tracing::warn!(
            path = %path.display(),
            "could not mark the file as downloaded: {e}"
        );
    }
}

/// Writes the `Zone.Identifier` stream of Internet zone (3).
#[cfg(windows)]
fn write_mark(path: &Path, url: Option<&str>, referer: Option<&str>) -> std::io::Result<()> {
    let mut stream = path.as_os_str().to_owned();
    stream.push(":Zone.Identifier");
    std::fs::write(
        std::path::PathBuf::from(stream),
        zone_identifier(url, referer),
    )
}

/// Sets the quarantine attribute, as a browser does.
#[cfg(target_os = "macos")]
fn write_mark(path: &Path, _url: Option<&str>, _referer: Option<&str>) -> std::io::Result<()> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    xattr::set(path, "com.apple.quarantine", quarantine(now).as_bytes())
}

/// Sets the xdg origin attributes, as browsers on Linux do.
#[cfg(target_os = "linux")]
fn write_mark(path: &Path, url: Option<&str>, referer: Option<&str>) -> std::io::Result<()> {
    if let Some(url) = url {
        xattr::set(path, "user.xdg.origin.url", url.as_bytes())?;
    }
    if let Some(referer) = referer {
        xattr::set(path, "user.xdg.referrer.url", referer.as_bytes())?;
    }
    Ok(())
}

#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
fn write_mark(_path: &Path, _url: Option<&str>, _referer: Option<&str>) -> std::io::Result<()> {
    Ok(())
}

/// `url` without user name and password, as browsers write it into these
/// marks; `None` if it does not parse.
fn without_credentials(url: &str) -> Option<String> {
    let mut url = url::Url::parse(url).ok()?;
    // Both fail only for URLs that cannot carry credentials anyway.
    let _ = url.set_username("");
    let _ = url.set_password(None);
    Some(url.into())
}

#[cfg(any(windows, test))]
fn zone_identifier(url: Option<&str>, referer: Option<&str>) -> String {
    let line = |key: &str, value: Option<&str>| {
        value.map_or_else(String::new, |v| format!("{key}={v}\r\n"))
    };
    format!(
        "[ZoneTransfer]\r\nZoneId=3\r\n{}{}",
        line("ReferrerUrl", referer),
        line("HostUrl", url)
    )
}

/// The quarantine value: flags, the time in hex, the agent.
#[cfg(any(target_os = "macos", test))]
fn quarantine(unix_secs: u64) -> String {
    format!("0081;{unix_secs:08x};Hyades;")
}

#[cfg(test)]
mod tests {
    use super::*;

    const URL: &str = "https://example.com/x.iso";
    const REFERER: &str = "https://example.com/downloads";

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_sets_xdg_origin() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.iso");
        std::fs::write(&path, b"x").unwrap();
        match write_mark(&path, Some(URL), Some(REFERER)) {
            Err(e)
                if matches!(e.raw_os_error(), Some(code)
                    if code == libc::ENOTSUP || code == libc::EOPNOTSUPP) =>
            {
                eprintln!("skipped: the temp dir's file system has no user xattrs ({e})");
                return;
            }
            other => other.unwrap(),
        }
        let get = |name| xattr::get(&path, name).unwrap();
        assert_eq!(get("user.xdg.origin.url").as_deref(), Some(URL.as_bytes()));
        assert_eq!(
            get("user.xdg.referrer.url").as_deref(),
            Some(REFERER.as_bytes())
        );
        // Without a referer only the origin is set, without credentials.
        let bare = dir.path().join("bare");
        std::fs::write(&bare, b"x").unwrap();
        mark_downloaded(&bare, "https://user:secret@example.com/x.iso", None);
        assert_eq!(
            xattr::get(&bare, "user.xdg.origin.url").unwrap().as_deref(),
            Some(URL.as_bytes())
        );
        assert_eq!(xattr::get(&bare, "user.xdg.referrer.url").unwrap(), None);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_sets_quarantine() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.iso");
        std::fs::write(&path, b"x").unwrap();
        write_mark(&path, Some(URL), Some(REFERER)).unwrap();
        let value = xattr::get(&path, "com.apple.quarantine")
            .unwrap()
            .expect("quarantine attribute is set");
        let value = String::from_utf8(value).unwrap();
        assert!(value.starts_with("0081;"), "{value}");
        assert!(value.contains(";Hyades;"), "{value}");
    }

    #[cfg(windows)]
    #[test]
    fn windows_writes_zone_identifier() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.iso");
        std::fs::write(&path, b"x").unwrap();
        write_mark(&path, Some(URL), Some(REFERER)).unwrap();
        let mut stream = path.as_os_str().to_owned();
        stream.push(":Zone.Identifier");
        let text = std::fs::read_to_string(std::path::PathBuf::from(stream)).unwrap();
        assert!(text.contains("ZoneId=3"), "{text}");
        assert!(text.contains(&format!("HostUrl={URL}")), "{text}");
        // The file itself is untouched.
        assert_eq!(std::fs::read(&path).unwrap(), b"x");
    }

    #[test]
    fn zone_identifier_format() {
        assert_eq!(
            zone_identifier(Some(URL), Some(REFERER)),
            format!("[ZoneTransfer]\r\nZoneId=3\r\nReferrerUrl={REFERER}\r\nHostUrl={URL}\r\n")
        );
        assert_eq!(
            zone_identifier(Some(URL), None),
            format!("[ZoneTransfer]\r\nZoneId=3\r\nHostUrl={URL}\r\n")
        );
    }

    #[test]
    fn credentials_are_left_out() {
        assert_eq!(
            without_credentials("https://user:pa%20ss@example.com:8443/a/x.iso?token=t#f")
                .as_deref(),
            Some("https://example.com:8443/a/x.iso?token=t#f")
        );
        assert_eq!(
            without_credentials("ftp://user@example.com/x.iso").as_deref(),
            Some("ftp://example.com/x.iso")
        );
        assert_eq!(without_credentials(URL).as_deref(), Some(URL));
        assert_eq!(without_credentials("not a url"), None);
    }

    #[test]
    fn quarantine_format() {
        assert_eq!(quarantine(0x65a1_b2c3), "0081;65a1b2c3;Hyades;");
        assert_eq!(quarantine(1), "0081;00000001;Hyades;");
    }
}
