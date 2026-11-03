//! `taurus grab`: the files a web page (or folder listing) links to, listed
//! at once; with `--sizes` their sizes as they arrive, and with `--add`
//! queued as downloads (never waiting on a size).

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
    /// After the listing, ask the server for each file's size and print one line per file (its index and size) as each answer arrives
    #[arg(long, conflicts_with = "add")]
    pub sizes: bool,
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

/// The page's address as the Referer of `link`, when both are on one origin
/// (scheme, host and port): what a browser sends when it follows the link,
/// without the page's `#fragment`. `None` for any other link, so an https
/// page's address (which may carry a token) never goes to a plain-http one.
fn referer_for(page: &str, link: &str) -> Option<String> {
    let (Ok(mut page), Ok(link)) = (url::Url::parse(page), url::Url::parse(link)) else {
        return None;
    };
    let same_origin = page.host_str().is_some()
        && page.scheme() == link.scheme()
        && page.host_str() == link.host_str()
        && page.port_or_known_default() == link.port_or_known_default();
    if !same_origin {
        return None;
    }
    page.set_fragment(None);
    Some(page.into())
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
        print!("{}", render::grab_links(&links));
    }
    if found.truncated && !json {
        eprintln!("note: the page has more links than are listed");
    }
    if args.sizes && !links.is_empty() {
        print_sizes(engine, &links, json).await;
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
                referer: referer_for(&page_url, &link.url),
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

/// Probes the links' sizes and prints each as it arrives: a line of index
/// and size, or with `json` an object `{"index", "size"}` per line.
async fn print_sizes(engine: &Engine, links: &[GrabbedLink], json: bool) {
    let mut sizes = engine.probe_sizes_stream(links.iter().map(|l| l.url.clone()).collect());
    if !json {
        println!("{}", render::GRAB_SIZES_HEADING);
    }
    while let Some((index, size)) = sizes.recv().await {
        if json {
            println!("{}", serde_json::json!({ "index": index, "size": size }));
        } else {
            println!("{}", render::grab_size(index, size));
        }
    }
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
    fn the_referer_is_for_the_pages_own_origin_without_its_fragment() {
        let page = "http://h.example/lst/";
        let sent = |link: &str| referer_for(page, link);
        assert_eq!(sent("http://h.example/lst/a.pdf").as_deref(), Some(page));
        assert_eq!(sent("http://h.example:80/x.pdf").as_deref(), Some(page));
        assert_eq!(sent("http://h.example:8080/x.pdf"), None);
        assert_eq!(sent("http://cdn.example/x.pdf"), None);
        assert_eq!(sent("not a url"), None);
        // Another scheme on the same host and port is another origin: an
        // https page's address never goes to a plain-http link.
        let secure = "https://h.example:8443/lst/?token=abc";
        assert_eq!(referer_for(secure, "http://h.example:8443/x.pdf"), None);
        assert_eq!(
            referer_for(secure, "https://h.example:8443/x.pdf").as_deref(),
            Some(secure)
        );
        // Browsers never send the fragment.
        assert_eq!(
            referer_for("http://h.example/lst/#top", "http://h.example/a.pdf").as_deref(),
            Some("http://h.example/lst/")
        );
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
