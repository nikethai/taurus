//! `taurus grab`: the files a web page (or folder listing) links to, with
//! their sizes, and with `--add` queued as downloads.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Args;
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
    /// Download every listed file, then run the queue until it is idle; Ctrl-C stops (taurus run continues)
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
            let req = AddRequest {
                url: link.url.clone(),
                dest_dir: dest_dir.clone(),
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
