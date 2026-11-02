//! `taurus grab`: the files a web page (or folder listing) links to, with
//! their sizes, and with `--add` queued as downloads.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::Args;
use taurus_core::filename::sanitize;
use taurus_core::{AddRequest, Engine, GrabOptions, GrabbedLink};

use crate::runner::{Enqueued, Options, Runner};
use crate::{Result, default_dest_dir, render};

#[derive(Args, Clone, Debug)]
pub struct GrabArgs {
    /// The web page or folder listing to read
    #[arg(value_name = "PAGE_URL")]
    pub url: String,
    /// A generated folder listing: also read its subfolders (one level)
    #[arg(long)]
    pub subfolders: bool,
    /// List only files with these extensions, e.g. pdf,zip
    #[arg(long, value_name = "EXT", value_delimiter = ',')]
    pub ext: Vec<String>,
    /// Download every listed file (each into its own subfolder of --dir, as listed), then run the queue until it is idle; Ctrl-C stops (taurus run continues)
    #[arg(long)]
    pub add: bool,
    /// With --add: the destination folder [default: the Downloads folder, else the current dir]
    #[arg(long, value_name = "PATH", requires = "add")]
    pub dir: Option<PathBuf>,
}

/// Whether `link` has one of the extensions (any, when none are asked
/// for); ignores case and a leading dot.
fn wanted(link: &GrabbedLink, exts: &[String]) -> bool {
    exts.is_empty()
        || link.extension.as_deref().is_some_and(|have| {
            exts.iter()
                .any(|e| e.trim().trim_start_matches('.').eq_ignore_ascii_case(have))
        })
}

/// Where the files of a link's `folder` (`/`, or `/talks/` for a scanned
/// subfolder) go: below `dest_dir`, one folder per segment. The names come
/// from a web page (percent-decoded, so they can hold `/`, a backslash or `..`):
/// a segment that is empty, `.`, `..` or holds a backslash is refused, the
/// others are made safe for a file system, and the result must still be
/// inside `dest_dir`.
fn folder_in(dest_dir: &Path, folder: &str) -> std::result::Result<PathBuf, String> {
    let unsafe_folder = || "a folder name on the page is not safe to create".to_string();
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

/// Whether both links are on one host and port.
fn same_host(a: &str, b: &str) -> bool {
    match (url::Url::parse(a), url::Url::parse(b)) {
        (Ok(a), Ok(b)) => {
            a.host_str().is_some()
                && a.host_str() == b.host_str()
                && a.port_or_known_default() == b.port_or_known_default()
        }
        _ => false,
    }
}

pub async fn run(engine: &Engine, args: GrabArgs, options: Options) -> Result<ExitCode> {
    let json = options.json;
    let found = engine
        .grab(
            &args.url,
            GrabOptions {
                subfolders: args.subfolders,
            },
        )
        .await?;
    let page_url = found.page_url;
    let links: Vec<GrabbedLink> = found
        .links
        .into_iter()
        .filter(|l| wanted(l, &args.ext))
        .collect();
    let sizes = if links.is_empty() {
        Vec::new()
    } else {
        engine
            .probe_sizes(links.iter().map(|l| l.url.clone()).collect())
            .await
    };
    if json {
        let rows: Vec<_> = links
            .iter()
            .enumerate()
            .map(|(index, link)| {
                serde_json::json!({
                    "index": index,
                    "url": link.url,
                    "name": link.name,
                    "folder": link.folder,
                    "extension": link.extension,
                    "size": sizes.get(index).copied().flatten(),
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::json!({ "links": rows, "truncated": found.truncated })
        );
    } else if links.is_empty() {
        println!("No files found.");
    } else {
        print!("{}", render::grab_links(&links, &sizes));
    }
    if found.truncated && !json {
        eprintln!("note: the page has more links than are listed");
    }
    if !args.add || links.is_empty() {
        return Ok(ExitCode::SUCCESS);
    }
    let dest_dir = std::path::absolute(match args.dir {
        Some(dir) => dir,
        None => default_dest_dir()?,
    })?;
    let runner = Runner::new(engine, options);
    let enqueue = async {
        let hold = engine.hold_when_done();
        let mut enqueued = Enqueued::default();
        for link in links {
            let folder = match folder_in(&dest_dir, &link.folder) {
                Ok(folder) => match tokio::fs::create_dir_all(&folder).await {
                    Ok(()) => Ok(folder),
                    Err(e) => Err(format!("cannot create {}: {e}", folder.display())),
                },
                Err(e) => Err(e),
            };
            let folder = match folder {
                Ok(folder) => folder,
                Err(e) => {
                    eprintln!("error: {}: {e}", link.url);
                    enqueued.failed = true;
                    continue;
                }
            };
            let req = AddRequest {
                url: link.url.clone(),
                dest_dir: folder,
                // What a browser sends when it follows the link, for hosts
                // that serve a file only from their own page.
                referer: same_host(&page_url, &link.url).then(|| page_url.clone()),
                ..AddRequest::default()
            };
            match Box::pin(engine.add(req)).await {
                Ok(id) => enqueued.ids.push(id),
                Err(e) => {
                    eprintln!("error: {}: {e}", link.url);
                    enqueued.failed = true;
                }
            }
        }
        drop(hold);
        enqueued
    };
    runner.run(enqueue).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn link(name: &str, extension: Option<&str>) -> GrabbedLink {
        GrabbedLink {
            url: format!("http://h/{name}"),
            name: name.to_string(),
            folder: "/".to_string(),
            extension: extension.map(String::from),
        }
    }

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
            assert!(error.contains("not safe"), "{hostile}: {error}");
        }
    }

    #[test]
    fn the_referer_is_for_the_pages_own_host() {
        let page = "http://h.example/lst/";
        assert!(same_host(page, "http://h.example/lst/a.pdf"));
        assert!(same_host(page, "http://h.example:80/x.pdf"));
        assert!(!same_host(page, "http://h.example:8080/x.pdf"));
        assert!(!same_host(page, "http://cdn.example/x.pdf"));
        assert!(!same_host(page, "not a url"));
    }

    #[test]
    fn extensions_filter_ignoring_case_and_dots() {
        let pdf = link("a.pdf", Some("pdf"));
        let none = link("README", None);
        let exts = |l: &[&str]| l.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(wanted(&pdf, &[]));
        assert!(wanted(&none, &[]));
        assert!(wanted(&pdf, &exts(&["zip", ".PDF"])));
        assert!(!wanted(&pdf, &exts(&["zip"])));
        assert!(!wanted(&none, &exts(&["pdf"])));
    }
}
