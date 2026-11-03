//! The pure part of grabbing: what links a page holds and which of them
//! are files or folders to read. HTML is parsed with `scraper` (html5ever),
//! which the engine already uses to recognize web pages: it decodes
//! entities and survives broken markup.

use std::sync::LazyLock;

use scraper::{Html, Selector};

/// Extensions of pages: a link to one is a page, not a file to download.
const PAGE_EXTENSIONS: [&str; 6] = ["html", "htm", "php", "asp", "aspx", "jsp"];

fn selector(css: &str) -> Selector {
    Selector::parse(css).expect("static selector")
}

/// The only elements and attributes links are read from.
static LINKS: LazyLock<Selector> =
    LazyLock::new(|| selector("a[href], img[src], video[src], audio[src], source[src]"));
static BASE: LazyLock<Selector> = LazyLock::new(|| selector("base[href]"));
static HEADINGS: LazyLock<Selector> = LazyLock::new(|| selector("title, h1"));
/// What Apache's and nginx's generated listings contain.
static LISTING_MARKERS: LazyLock<Selector> = LazyLock::new(|| {
    selector(
        "table#indexlist, td.indexcolname, img[alt=\"[PARENTDIR]\"], img[alt=\"[DIR]\"], \
         pre > a[href=\"../\"]",
    )
});

/// The http(s) links of `a[href]`, `img[src]`, `video[src]`, `audio[src]` and
/// `source[src]`, resolved against the page (or its `<base href>`), without
/// fragments, each once, in document order.
pub fn extract_links(page_url: &url::Url, html: &str) -> Vec<url::Url> {
    let doc = Html::parse_document(html);
    let base = doc
        .select(&BASE)
        .next()
        .and_then(|b| b.attr("href"))
        .and_then(|href| page_url.join(href.trim()).ok())
        .filter(|base| matches!(base.scheme(), "http" | "https"))
        .unwrap_or_else(|| page_url.clone());
    let mut seen = std::collections::HashSet::new();
    let mut links = Vec::new();
    for element in doc.select(&LINKS) {
        let attr = if element.value().name() == "a" {
            "href"
        } else {
            "src"
        };
        let Some(value) = element.attr(attr) else {
            continue;
        };
        let Ok(mut link) = base.join(value.trim()) else {
            continue;
        };
        if !matches!(link.scheme(), "http" | "https") {
            continue;
        }
        link.set_fragment(None);
        if seen.insert(link.clone()) {
            links.push(link);
        }
    }
    links
}

/// A generated directory listing: the `<title>` or an `<h1>` starts with
/// "Index of" (any case), or it has Apache's index table or parent-directory
/// icons, or nginx's `<pre>` list starting with a `../` link.
pub fn is_folder_listing(html: &str) -> bool {
    let doc = Html::parse_document(html);
    let titled = doc.select(&HEADINGS).any(|heading| {
        let text: String = heading.text().collect();
        let text = text.trim_start();
        text.get(.."index of".len())
            .is_some_and(|start| start.eq_ignore_ascii_case("index of"))
    });
    titled || doc.select(&LISTING_MARKERS).next().is_some()
}

/// Whether `candidate` is below `page_dir` on the same host: same scheme,
/// host and port, a path that starts with the directory's path (taken as
/// ending in `/`) and goes
/// further (the directory itself, with or without a query, is not inside),
/// and no `..` segment.
pub fn is_inside(page_dir: &url::Url, candidate: &url::Url) -> bool {
    if page_dir.scheme() != candidate.scheme()
        || page_dir.host() != candidate.host()
        || page_dir.port_or_known_default() != candidate.port_or_known_default()
    {
        return false;
    }
    // `/files` names the directory `/files/`, not `/files-old/`.
    let dir = match page_dir.path() {
        dir if dir.ends_with('/') => std::borrow::Cow::Borrowed(dir),
        dir => std::borrow::Cow::Owned(format!("{dir}/")),
    };
    let path = candidate.path();
    path.len() > dir.len()
        && path.starts_with(dir.as_ref())
        && !path
            .split('/')
            .any(|segment| segment == ".." || segment.eq_ignore_ascii_case("%2e%2e"))
}

/// The last path segment has an extension that is not a page's (`.html`,
/// `.htm`, `.php`, `.asp`, `.aspx`, `.jsp`). A folder (path ending in `/`)
/// and a hidden file with no name before its dot are not files.
pub fn looks_like_file(u: &url::Url) -> bool {
    extension(u).is_some_and(|ext| !PAGE_EXTENSIONS.contains(&ext.as_str()))
}

/// The lower-case extension of the last path segment, if it has one.
pub(crate) fn extension(u: &url::Url) -> Option<String> {
    let segment = u.path().rsplit('/').next()?;
    let (stem, ext) = segment.rsplit_once('.')?;
    let valid =
        !stem.is_empty() && !ext.is_empty() && ext.chars().all(|c| c.is_ascii_alphanumeric());
    valid.then(|| ext.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(s: &str) -> url::Url {
        url::Url::parse(s).unwrap()
    }

    fn strings(links: Vec<url::Url>) -> Vec<String> {
        links.into_iter().map(String::from).collect()
    }

    #[test]
    fn relative_links_resolved() {
        let page = url("http://h.test/files/index.html");
        let html = r#"<a href="a.zip">a</a> <a href="/b.zip">b</a> <a href="../c.zip">c</a>
            <a href="//other.test/d.zip">d</a> <a href="sub/">s</a> <a href="mailto:x@y.z">m</a>
            <a href="javascript:void(0)">j</a> <a>no href</a> <a href="  e.zip ">e</a>"#;
        assert_eq!(
            strings(extract_links(&page, html)),
            [
                "http://h.test/files/a.zip",
                "http://h.test/b.zip",
                "http://h.test/c.zip",
                "http://other.test/d.zip",
                "http://h.test/files/sub/",
                "http://h.test/files/e.zip",
            ]
        );
    }

    #[test]
    fn base_href_and_entities() {
        let page = url("http://h.test/files/");
        let html = r#"<head><base href="/mirror/"></head>
            <a href="a.zip?x=1&amp;y=2">a</a> <a href="b&#46;zip">b</a>"#;
        assert_eq!(
            strings(extract_links(&page, html)),
            [
                "http://h.test/mirror/a.zip?x=1&y=2",
                "http://h.test/mirror/b.zip"
            ]
        );
        // A relative base is resolved against the page; a bad one is ignored.
        let html = r#"<base href="deeper/"><a href="a.zip">a</a>"#;
        assert_eq!(
            strings(extract_links(&page, html)),
            ["http://h.test/files/deeper/a.zip"]
        );
        let html = r#"<base href="http://[bad"><a href="a.zip">a</a>"#;
        assert_eq!(
            strings(extract_links(&page, html)),
            ["http://h.test/files/a.zip"]
        );
    }

    #[test]
    fn fragments_and_duplicates_removed() {
        let page = url("http://h.test/p/");
        let html = r##"<a href="a.zip#top">1</a> <a href="a.zip">2</a> <a href="b.zip#x">3</a>
            <a href="a.zip#other">4</a>"##;
        assert_eq!(
            strings(extract_links(&page, html)),
            ["http://h.test/p/a.zip", "http://h.test/p/b.zip"]
        );
    }

    #[test]
    fn media_and_image_sources_included() {
        let page = url("http://h.test/p/");
        let html = r#"<img src="i.png"><video src="v.mp4"><source src="v.webm"></video>
            <audio src="a.mp3"></audio><a href="d.pdf">d</a>
            <script src="s.js"></script><link href="s.css" rel="stylesheet">
            <iframe src="f.html"></iframe><img srcset="x.png 2x">"#;
        assert_eq!(
            strings(extract_links(&page, html)),
            [
                "http://h.test/p/i.png",
                "http://h.test/p/v.mp4",
                "http://h.test/p/v.webm",
                "http://h.test/p/a.mp3",
                "http://h.test/p/d.pdf",
            ]
        );
    }

    #[test]
    fn index_of_detected() {
        assert!(is_folder_listing(
            "<html><head><title>Index of /pub</title></head><body></body></html>"
        ));
        assert!(is_folder_listing(
            "<html><body><h1>  index of /pub/x</h1><hr><pre></pre></body></html>"
        ));
        // Apache's table and nginx's parent link, with a different title.
        assert!(is_folder_listing(
            r#"<title>Files</title><table id="indexlist"><tr></tr></table>"#
        ));
        assert!(is_folder_listing(
            r#"<title>Files</title><pre><a href="../">../</a>
<a href="a.zip">a.zip</a></pre>"#
        ));
        assert!(!is_folder_listing(
            "<title>My Index of things</title><h1>Welcome</h1><a href=\"a.zip\">a</a>"
        ));
        assert!(!is_folder_listing(
            "<html><body>Index of nothing</body></html>"
        ));
    }

    #[test]
    fn inside_rules() {
        let dir = url("http://h.test/files/");
        assert!(is_inside(&dir, &url("http://h.test/files/a.zip")));
        assert!(is_inside(&dir, &url("http://h.test/files/sub/")));
        assert!(is_inside(&dir, &url("http://h.test/files/sub/deep/a.zip")));
        // The directory itself (with or without a query) is not inside it.
        assert!(!is_inside(&dir, &url("http://h.test/files/")));
        assert!(!is_inside(&dir, &url("http://h.test/files/?C=N;O=D")));
        // Up, other directories, the root.
        assert!(!is_inside(&dir, &url("http://h.test/")));
        assert!(!is_inside(&dir, &url("http://h.test/up/")));
        assert!(!is_inside(&dir, &url("http://h.test/files/../up/")));
        assert!(!is_inside(&dir, &url("http://h.test/files/%2e%2e/up/")));
        assert!(!is_inside(&dir, &url("http://h.test/files")));
        assert!(!is_inside(&dir, &url("http://h.test/files-old/a.zip")));
        // Same host only: scheme, host and port all count.
        assert!(!is_inside(&dir, &url("https://h.test/files/a.zip")));
        assert!(!is_inside(&dir, &url("http://other.test/files/a.zip")));
        assert!(!is_inside(&dir, &url("http://h.test:8080/files/a.zip")));
        assert!(is_inside(&dir, &url("http://H.test:80/files/a.zip")));
        assert!(!is_inside(&dir, &url("ftp://h.test/files/a.zip")));
        // A relative `sub/` resolved against the directory.
        assert!(is_inside(&dir, &dir.join("sub/").unwrap()));
        // A directory given without its trailing slash is still a directory.
        let bare = url("http://h.test/files");
        assert!(is_inside(&bare, &url("http://h.test/files/a.zip")));
        assert!(!is_inside(&bare, &url("http://h.test/files-old/a.zip")));
        assert!(!is_inside(&bare, &url("http://h.test/files")));
        assert!(!is_inside(&bare, &url("http://h.test/files/")));
        assert!(!is_inside(&dir, &dir.join("../up/").unwrap()));
        assert!(!is_inside(&dir, &dir.join("/").unwrap()));
    }

    #[test]
    fn looks_like_file_cases() {
        for yes in [
            "http://h.test/a.zip",
            "http://h.test/x/y/Movie.MKV",
            "http://h.test/a.tar.gz",
            "http://h.test/a.zip?token=1",
            "http://h.test/my%20file.pdf",
            "http://h.test/htmlpage.txt",
        ] {
            assert!(looks_like_file(&url(yes)), "{yes}");
        }
        for no in [
            "http://h.test/",
            "http://h.test/dir/",
            "http://h.test/noext",
            "http://h.test/a.html",
            "http://h.test/a.HTM",
            "http://h.test/a.php",
            "http://h.test/a.asp",
            "http://h.test/a.aspx",
            "http://h.test/a.jsp",
            "http://h.test/.hidden",
            "http://h.test/trailing.",
            "http://h.test/dir.d/",
        ] {
            assert!(!looks_like_file(&url(no)), "{no}");
        }
    }
}
