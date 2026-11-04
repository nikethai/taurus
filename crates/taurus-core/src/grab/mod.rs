//! Grabbing the links of a web page (or of an Apache/nginx folder listing and
//! its subfolders) so a front end can offer them for download. The page's HTML
//! is read in [`parse`]; fetching is `Engine::grab` (engine/grab.rs).

pub mod parse;

use std::path::{Path, PathBuf};

use crate::errors::{CoreError, ErrorKind};
use crate::filename::sanitize;

/// The most links one grab returns.
pub const MAX_LINKS: usize = 500;

/// The most HTML read from one page.
pub const MAX_PAGE_BYTES: usize = 5 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct GrabbedLink {
    pub url: String,
    /// The last path segment, percent-decoded.
    pub name: String,
    /// "/" for the page itself, "/talks/" for a scanned subfolder: relative
    /// to the page's directory.
    pub folder: String,
    /// Lower-case, without the dot.
    pub extension: Option<String>,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct GrabResult {
    /// The page as it was read (after redirects).
    pub page_url: String,
    pub links: Vec<GrabbedLink>,
    /// Subfolders that were read and added to `links`.
    pub subfolders_scanned: u32,
    /// Links were dropped: past [`MAX_LINKS`], or the page is longer than
    /// [`MAX_PAGE_BYTES`] and was read up to there.
    pub truncated: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GrabOptions {
    /// When the page is a folder listing, also read the subfolders it links
    /// to (one level, inside the page's own directory).
    pub subfolders: bool,
}

/// Where the files of a link's `folder` (`/`, or `/talks/` for a scanned
/// subfolder) go: below `dest_dir`, one folder per segment. The names come
/// from a web page (percent-decoded, so they can hold `/`, a backslash or
/// `..`): a segment that is empty, `.`, `..` or holds a backslash is
/// refused, the others are made safe for a file system
/// ([`sanitize`]), and the result must still be inside `dest_dir`.
/// Refusals are `Unknown`, with the detail "a folder name on the page is
/// not safe to create". Nothing is created.
pub fn folder_in(dest_dir: &Path, folder: &str) -> Result<PathBuf, CoreError> {
    let unsafe_folder = || CoreError {
        kind: ErrorKind::Unknown,
        detail: "a folder name on the page is not safe to create".to_string(),
    };
    let mut path = dest_dir.to_path_buf();
    let relative = folder.trim_matches('/');
    if relative.is_empty() {
        return Ok(path);
    }
    for segment in relative.split('/') {
        if matches!(segment, "" | "." | "..") || segment.contains('\\') {
            return Err(unsafe_folder());
        }
        path.push(sanitize(segment));
    }
    if path.starts_with(dest_dir) {
        Ok(path)
    } else {
        Err(unsafe_folder())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folders_stay_inside_the_destination() {
        let dest = Path::new("/data/dl");
        assert_eq!(folder_in(dest, "/").unwrap(), dest);
        assert_eq!(folder_in(dest, "/2023/").unwrap(), dest.join("2023"));
        assert_eq!(folder_in(dest, "/a/b/").unwrap(), dest.join("a").join("b"));
        // A name the file system would not take is made safe.
        assert_eq!(folder_in(dest, "/a:b/").unwrap(), dest.join("a_b"));
        for hostile in [
            "/../",
            "/x/../../escape/",
            "/a//b/",
            "/./",
            "/a\\b/",
            "/..\\..\\x/",
            "/ok/../",
        ] {
            let error = folder_in(dest, hostile).unwrap_err();
            assert_eq!(error.kind, ErrorKind::Unknown, "{hostile}");
            assert!(error.detail.contains("not safe"), "{hostile}: {error}");
        }
    }
}
