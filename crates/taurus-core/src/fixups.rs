//! Rewrites share links (Dropbox, GitHub blob pages, Google Drive file
//! pages) to direct-download URLs before the engine resolves them.

use url::Url;

/// Returns the direct-download form of a known share link, or `None` when the
/// URL needs no rewriting.
pub fn rewrite(url: &Url) -> Option<Url> {
    match url.host_str()? {
        "www.dropbox.com" | "dropbox.com" => dropbox(url),
        "github.com" | "www.github.com" => github(url),
        "drive.google.com" => google_drive(url),
        _ => None,
    }
}

fn dropbox(url: &Url) -> Option<Url> {
    let mut pairs: Vec<(String, String)> = url
        .query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    let mut found = false;
    for (key, value) in &mut pairs {
        if key == "dl" {
            *value = "1".to_owned();
            found = true;
        }
    }
    if !found {
        pairs.push(("dl".to_owned(), "1".to_owned()));
    }
    let mut out = url.clone();
    out.query_pairs_mut().clear().extend_pairs(pairs);
    Some(out)
}

/// `/<owner>/<repo>/blob/<ref>/<path...>` becomes the raw-content URL.
fn github(url: &Url) -> Option<Url> {
    let segments: Vec<&str> = url.path_segments()?.collect();
    if segments.len() < 5 || segments[2] != "blob" || segments.iter().any(|s| s.is_empty()) {
        return None;
    }
    let path = [&segments[..2], &segments[3..]].concat().join("/");
    Url::parse(&format!("https://raw.githubusercontent.com/{path}")).ok()
}

/// `/file/d/<ID>/...`, `/open?id=<ID>` and `/uc?id=<ID>`.
fn google_drive(url: &Url) -> Option<Url> {
    let segments: Vec<&str> = url.path_segments()?.collect();
    let id = match segments.as_slice() {
        ["file", "d", id, ..] => (*id).to_owned(),
        ["open"] | ["uc"] => url
            .query_pairs()
            .find(|(k, _)| k == "id")
            .map(|(_, v)| v.into_owned())?,
        _ => return None,
    };
    if id.is_empty() {
        return None;
    }
    let mut out = Url::parse("https://drive.usercontent.google.com/download").ok()?;
    out.query_pairs_mut()
        .append_pair("id", &id)
        .append_pair("export", "download");
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fix(url: &str) -> Option<String> {
        rewrite(&Url::parse(url).unwrap()).map(|u| u.to_string())
    }

    #[test]
    fn dropbox_dl0_becomes_dl1() {
        assert_eq!(
            fix("https://www.dropbox.com/s/abc123/file.zip?dl=0").as_deref(),
            Some("https://www.dropbox.com/s/abc123/file.zip?dl=1")
        );
    }

    #[test]
    fn dropbox_without_query_gets_dl1() {
        assert_eq!(
            fix("https://dropbox.com/s/abc123/file.zip").as_deref(),
            Some("https://dropbox.com/s/abc123/file.zip?dl=1")
        );
    }

    #[test]
    fn dropbox_keeps_other_params() {
        assert_eq!(
            fix("https://www.dropbox.com/scl/fi/x/file.zip?rlkey=abc&dl=0").as_deref(),
            Some("https://www.dropbox.com/scl/fi/x/file.zip?rlkey=abc&dl=1")
        );
        assert_eq!(
            fix("https://www.dropbox.com/scl/fi/x/file.zip?rlkey=abc").as_deref(),
            Some("https://www.dropbox.com/scl/fi/x/file.zip?rlkey=abc&dl=1")
        );
    }

    #[test]
    fn github_blob_becomes_raw() {
        assert_eq!(
            fix("https://github.com/owner/repo/blob/main/dir/tool.tar.gz").as_deref(),
            Some("https://raw.githubusercontent.com/owner/repo/main/dir/tool.tar.gz")
        );
        assert_eq!(
            fix("https://www.github.com/o/r/blob/v1.0/a.bin?raw=false#L3").as_deref(),
            Some("https://raw.githubusercontent.com/o/r/v1.0/a.bin")
        );
    }

    #[test]
    fn github_non_blob_pages_unchanged() {
        assert_eq!(fix("https://github.com/owner/repo"), None);
        assert_eq!(fix("https://github.com/owner/repo/tree/main/dir"), None);
        assert_eq!(fix("https://github.com/owner/repo/blob/main"), None);
        assert_eq!(fix("https://github.com/owner/repo/blob/main/"), None);
        assert_eq!(
            fix("https://github.com/owner/repo/releases/download/v1/a.zip"),
            None
        );
    }

    #[test]
    fn google_drive_file_page() {
        assert_eq!(
            fix("https://drive.google.com/file/d/1AbC_-xyz/view?usp=sharing").as_deref(),
            Some("https://drive.usercontent.google.com/download?id=1AbC_-xyz&export=download")
        );
    }

    #[test]
    fn google_drive_open_and_uc() {
        let expected =
            Some("https://drive.usercontent.google.com/download?id=FILEID&export=download");
        assert_eq!(
            fix("https://drive.google.com/open?id=FILEID").as_deref(),
            expected
        );
        assert_eq!(
            fix("https://drive.google.com/uc?export=download&id=FILEID").as_deref(),
            expected
        );
        assert_eq!(
            fix("https://drive.google.com/uc?id=FILEID").as_deref(),
            expected
        );
    }

    #[test]
    fn google_drive_without_id_unchanged() {
        assert_eq!(fix("https://drive.google.com/drive/my-drive"), None);
        assert_eq!(fix("https://drive.google.com/open"), None);
        assert_eq!(fix("https://drive.google.com/uc?id="), None);
    }

    #[test]
    fn unrelated_url_unchanged() {
        assert_eq!(fix("https://example.org/a.zip"), None);
    }
}
