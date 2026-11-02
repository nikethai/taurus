//! Grabbing the links of a web page (or of an Apache/nginx folder listing and
//! its subfolders) so a front end can offer them for download. The page's HTML
//! is read in [`parse`]; fetching is `Engine::grab` (engine/grab.rs).

pub mod parse;

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
