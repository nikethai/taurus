//! A media job's picture: downloaded once, after inspection, for the UI.

use std::path::{Path, PathBuf};

use crate::errors::{CoreError, ErrorKind};
use crate::http::client::{request_error, status_error};

/// The thumbnails' folder under the data dir.
pub(crate) const THUMBS_DIR: &str = "thumbs";
/// A larger picture is not kept.
pub(crate) const MAX_THUMBNAIL: u64 = 2 * 1024 * 1024;
/// The picture types kept, with their file extensions.
const TYPES: [(&str, &str); 4] = [
    ("image/jpeg", "jpg"),
    ("image/png", "png"),
    ("image/webp", "webp"),
    ("image/gif", "gif"),
];

/// Downloads the picture at `url` to `dir/<stem>.<ext>`: only a JPEG, PNG,
/// WebP or GIF (by its content type) of at most 2 MiB. Returns its path.
pub(crate) async fn fetch_thumbnail(
    client: &reqwest::Client,
    url: &str,
    dir: &Path,
    stem: &str,
) -> Result<PathBuf, CoreError> {
    let mut response = client.get(url).send().await.map_err(request_error)?;
    if !response.status().is_success() {
        return Err(status_error(response.status()));
    }
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(';').next())
        .map(|v| v.trim().to_ascii_lowercase())
        .unwrap_or_default();
    let Some((_, ext)) = TYPES.iter().find(|(t, _)| *t == content_type) else {
        return Err(refused(format!("not a picture: {content_type:?}")));
    };
    if response.content_length().is_some_and(|n| n > MAX_THUMBNAIL) {
        return Err(refused("the picture is too large".to_string()));
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(request_error)? {
        if (body.len() + chunk.len()) as u64 > MAX_THUMBNAIL {
            return Err(refused("the picture is too large".to_string()));
        }
        body.extend_from_slice(&chunk);
    }
    let path = dir.join(format!("{stem}.{ext}"));
    let temp = dir.join(format!("{stem}.{ext}.tmp"));
    let written = async {
        tokio::fs::create_dir_all(dir).await?;
        tokio::fs::write(&temp, &body).await?;
        tokio::fs::rename(&temp, &path).await
    };
    if let Err(e) = written.await {
        let _ = tokio::fs::remove_file(&temp).await;
        return Err(CoreError {
            kind: crate::errors::classify_io(&e),
            detail: e.to_string(),
        });
    }
    Ok(path)
}

/// The picture saved for `stem` in `dir`, if there is one.
pub(crate) fn thumbnail_in(dir: &Path, stem: &str) -> Option<PathBuf> {
    TYPES
        .iter()
        .map(|(_, ext)| dir.join(format!("{stem}.{ext}")))
        .find(|p| p.is_file())
}

fn refused(detail: String) -> CoreError {
    CoreError {
        kind: ErrorKind::Unknown,
        detail,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use taurus_testkit::TestServer;

    #[tokio::test]
    async fn only_small_pictures_are_kept() {
        let server = TestServer::start().await;
        let tmp = tempfile::tempdir().unwrap();
        let client = reqwest::Client::new();
        let picture = server.serve_bytes("/t.png", "image/png", b"\x89PNG...");
        let path = fetch_thumbnail(&client, &picture, tmp.path(), "7")
            .await
            .unwrap();
        assert_eq!(path, tmp.path().join("7.png"));
        assert_eq!(std::fs::read(&path).unwrap(), b"\x89PNG...");
        assert_eq!(thumbnail_in(tmp.path(), "7"), Some(path));

        let jpeg = server.serve_bytes("/t.jpg", "image/jpeg; charset=binary", b"jpg");
        let path = fetch_thumbnail(&client, &jpeg, tmp.path(), "8")
            .await
            .unwrap();
        assert_eq!(path, tmp.path().join("8.jpg"));

        let page = server.serve_bytes("/t.html", "text/html", b"<html>");
        assert!(
            fetch_thumbnail(&client, &page, tmp.path(), "9")
                .await
                .is_err()
        );
        let svg = server.serve_bytes("/t.svg", "image/svg+xml", b"<svg/>");
        assert!(
            fetch_thumbnail(&client, &svg, tmp.path(), "9")
                .await
                .is_err()
        );
        let big = vec![0u8; MAX_THUMBNAIL as usize + 1];
        let large = server.serve_bytes("/big.png", "image/png", &big);
        assert!(
            fetch_thumbnail(&client, &large, tmp.path(), "9")
                .await
                .is_err()
        );
        let exact = vec![0u8; MAX_THUMBNAIL as usize];
        let fits = server.serve_bytes("/fits.png", "image/png", &exact);
        assert!(
            fetch_thumbnail(&client, &fits, tmp.path(), "10")
                .await
                .is_ok()
        );
        // Nothing was kept of the refused ones.
        assert_eq!(thumbnail_in(tmp.path(), "9"), None);
        assert_eq!(thumbnail_in(tmp.path(), "missing"), None);
    }
}
