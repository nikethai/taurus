//! `Engine::grab` and `Engine::probe_sizes` (or `probe_sizes_stream`):
//! reading the links of a web page (or folder listing) and the sizes of the
//! files they name. `grab` itself never probes a size. Both go through
//! the engine's HTTP client, so its settings (proxy, User-Agent, redirects)
//! apply to them as to every download.

use std::collections::HashSet;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use reqwest::header::CONTENT_TYPE;
use tokio::sync::{Semaphore, mpsc};
use tokio::task::JoinSet;

use crate::errors::{CoreError, ErrorKind};
use crate::grab::parse::{extension, extract_links, is_folder_listing, is_inside, looks_like_file};
use crate::grab::{GrabOptions, GrabResult, GrabbedLink, MAX_LINKS, MAX_PAGE_BYTES};
use crate::http::client::{bot_check_error, request_error, status_error};
use crate::http::probe;
use crate::page;

use super::Engine;

/// A page or listing must arrive (headers and body) within this long.
const PAGE_TIMEOUT: Duration = Duration::from_secs(60);
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);
/// Size probes in flight at once.
const PROBE_CONCURRENCY: usize = 4;
/// Subfolders read at once.
const FOLDER_CONCURRENCY: usize = 4;
/// The most subfolders read for one grab; more sets `truncated`.
const MAX_SUBFOLDERS: usize = 200;

impl Engine {
    /// Reads the web page at `page_url` and returns the files it links to:
    /// links of `a[href]`, `img[src]`, `video[src]`, `audio[src]` and
    /// `source[src]` (resolved against the page or its `<base href>`)
    /// whose last path segment has an extension that is not a web page's.
    /// At most 500 links are returned (`truncated` says more were found), in
    /// page order, each once. At most 5 MiB of the page is read; a longer
    /// page is cut there and `truncated` is set too.
    ///
    /// A page that is not HTML (by its Content-Type, or by its start when
    /// the server names none) fails with `SiteNotSupported` and the detail
    /// "not a web page"; an HTTP error or a bot check fails as it does for a
    /// download.
    ///
    /// With `opts.subfolders`, when the page is a generated folder listing
    /// ("Index of ..."), the subfolders it links to are read too, one level
    /// deep, and their files are listed with `folder` set to the subfolder.
    /// Only subfolders inside the page's own directory on the same host are
    /// read (never `../`, `/`, another host or scheme), and only files
    /// inside that subfolder are taken from it. A subfolder that fails to
    /// read is skipped.
    pub async fn grab(&self, page_url: &str, opts: GrabOptions) -> Result<GrabResult, CoreError> {
        self.check_running()?;
        let requested = parse_page_url(page_url)?;
        let page = read_page(&self.client.get(), &requested).await?;
        let cut = page.cut;
        let page_url = page.url.clone();
        let want_listing = opts.subfolders;
        let (links, listing) = tokio::task::spawn_blocking(move || {
            let links = extract_links(&page.url, &page.html);
            (links, want_listing && is_folder_listing(&page.html))
        })
        .await
        .map_err(|e| error(format!("reading the page failed: {e}")))?;
        let page_dir = page_directory(&page_url);
        let mut found = Found::new(cut);
        for link in links.iter().filter(|l| looks_like_file(l)) {
            found.add(link, "/");
        }
        let mut subfolders_scanned = 0;
        if listing {
            let mut folders = subfolders(&page_dir, &links);
            if folders.len() > MAX_SUBFOLDERS {
                folders.truncate(MAX_SUBFOLDERS);
                found.truncated = true;
            }
            for batch in folders.chunks(FOLDER_CONCURRENCY) {
                if found.full() {
                    // More to read, no room for it.
                    found.truncated = true;
                    break;
                }
                let client = self.client.clone();
                let dir = page_dir.clone();
                let read = run_bounded(batch.to_vec(), FOLDER_CONCURRENCY, move |folder| {
                    read_folder(client.get(), dir.clone(), folder)
                })
                .await;
                for (folder, links, cut) in read.into_iter().flatten().flatten() {
                    subfolders_scanned += 1;
                    found.truncated |= cut;
                    let label = folder_label(&page_dir, &folder);
                    for link in links
                        .iter()
                        .filter(|l| looks_like_file(l) && is_inside(&folder, l))
                    {
                        found.add(link, &label);
                    }
                }
            }
        }
        Ok(GrabResult {
            page_url: page_url.to_string(),
            links: found.links,
            subfolders_scanned,
            truncated: found.truncated,
        })
    }

    /// The size of each URL's file, in the order given: `None` for a URL
    /// that fails, is not an http(s) link, tells no size or does not answer
    /// within 10 seconds. Probes like `add` does (the first byte is asked
    /// for, never the body), four at a time.
    pub async fn probe_sizes(&self, urls: Vec<String>) -> Vec<Option<u64>> {
        probe_sizes_with(&self.client.get(), urls, PROBE_TIMEOUT).await
    }

    /// `probe_sizes`, each size sent as soon as it is known, as `(index of
    /// the URL, size)`, in the order they arrive; the stream ends once every
    /// URL has its answer. Same probes, same client, same timeout, four at a
    /// time. Dropping the receiver stops the probes. Must be called within
    /// a Tokio runtime.
    pub fn probe_sizes_stream(&self, urls: Vec<String>) -> mpsc::Receiver<(usize, Option<u64>)> {
        probe_stream_with(self.client.get(), urls, PROBE_TIMEOUT)
    }
}

/// One URL's size, as `probe_sizes` answers it.
async fn probe_size(client: &reqwest::Client, url: &str, timeout: Duration) -> Option<u64> {
    let probed = tokio::time::timeout(timeout, probe(client, url)).await;
    probed.ok()?.ok()?.size
}

async fn probe_sizes_with(
    client: &reqwest::Client,
    urls: Vec<String>,
    timeout: Duration,
) -> Vec<Option<u64>> {
    let client = client.clone();
    let sizes = run_bounded(urls, PROBE_CONCURRENCY, move |url| {
        let client = client.clone();
        async move { probe_size(&client, &url, timeout).await }
    })
    .await;
    sizes.into_iter().map(Option::flatten).collect()
}

fn probe_stream_with(
    client: reqwest::Client,
    urls: Vec<String>,
    timeout: Duration,
) -> mpsc::Receiver<(usize, Option<u64>)> {
    // Room for every answer: the probes never wait on a slow reader.
    let (tx, rx) = mpsc::channel(urls.len().max(1));
    tokio::spawn(async move {
        let permits = Arc::new(Semaphore::new(PROBE_CONCURRENCY));
        let mut tasks = JoinSet::new();
        for (index, url) in urls.into_iter().enumerate() {
            let (permits, client) = (permits.clone(), client.clone());
            tasks.spawn(async move {
                let _permit = permits.acquire().await.ok();
                (index, probe_size(&client, &url, timeout).await)
            });
        }
        loop {
            tokio::select! {
                // Nobody listens any more: dropping `tasks` stops them.
                () = tx.closed() => return,
                joined = tasks.join_next() => match joined {
                    Some(Ok(answer)) => {
                        if tx.send(answer).await.is_err() {
                            return;
                        }
                    }
                    // A probe that panicked has no answer.
                    Some(Err(_)) => {}
                    None => return,
                },
            }
        }
    });
    rx
}

/// Runs `f` on every item, `limit` at a time, and returns the results in the
/// order of the items (`None` for a task that panicked). The tasks stop if
/// the caller is dropped.
async fn run_bounded<I, T, F, Fut>(items: Vec<I>, limit: usize, f: F) -> Vec<Option<T>>
where
    I: Send + 'static,
    T: Send + 'static,
    F: Fn(I) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = T> + Send + 'static,
{
    let permits = Arc::new(Semaphore::new(limit));
    let f = Arc::new(f);
    let mut results: Vec<Option<T>> = items.iter().map(|_| None).collect();
    let mut tasks = JoinSet::new();
    for (index, item) in items.into_iter().enumerate() {
        let (permits, f) = (permits.clone(), f.clone());
        tasks.spawn(async move {
            let _permit = permits.acquire().await.ok();
            (index, f(item).await)
        });
    }
    while let Some(done) = tasks.join_next().await {
        if let Ok((index, value)) = done {
            results[index] = Some(value);
        }
    }
    results
}

/// The links grabbed so far: each URL once, at most [`MAX_LINKS`].
struct Found {
    links: Vec<GrabbedLink>,
    seen: HashSet<url::Url>,
    truncated: bool,
}

impl Found {
    fn new(truncated: bool) -> Found {
        Found {
            links: Vec::new(),
            seen: HashSet::new(),
            truncated,
        }
    }

    fn full(&self) -> bool {
        self.links.len() >= MAX_LINKS
    }

    fn add(&mut self, link: &url::Url, folder: &str) {
        if self.seen.contains(link) {
            return;
        }
        if self.full() {
            self.truncated = true;
            return;
        }
        self.seen.insert(link.clone());
        let segment = link.path().rsplit('/').next().unwrap_or_default();
        self.links.push(GrabbedLink {
            url: link.to_string(),
            name: percent_encoding::percent_decode_str(segment)
                .decode_utf8_lossy()
                .into_owned(),
            folder: folder.to_string(),
            extension: extension(link),
        });
    }
}

/// A fetched page: its URL after redirects, its text (at most
/// [`MAX_PAGE_BYTES`] of it, `cut` if there was more).
struct Page {
    url: url::Url,
    html: String,
    cut: bool,
}

fn error(detail: impl Into<String>) -> CoreError {
    CoreError {
        kind: ErrorKind::Unknown,
        detail: detail.into(),
    }
}

fn not_a_web_page() -> CoreError {
    CoreError {
        kind: ErrorKind::SiteNotSupported,
        detail: "not a web page".to_string(),
    }
}

fn parse_page_url(input: &str) -> Result<url::Url, CoreError> {
    let url =
        url::Url::parse(input.trim()).map_err(|e| error(format!("invalid URL {input:?}: {e}")))?;
    if matches!(url.scheme(), "http" | "https") {
        Ok(url)
    } else {
        Err(error("only http and https pages can be read"))
    }
}

/// `page_url` up to and including its last `/`, without query or fragment.
fn directory_of(page_url: &url::Url) -> url::Url {
    let mut dir = page_url.clone();
    dir.set_query(None);
    dir.set_fragment(None);
    let end = dir.path().rfind('/').map_or(0, |i| i + 1);
    let path = dir.path()[..end].to_string();
    dir.set_path(&path);
    dir
}

/// The directory whose folders a listing at `page_url` may have: the URL
/// itself when its last segment has no extension (a listing served at `/dir`
/// without a redirect is `/dir/`, so its folders are not the root's),
/// otherwise [`directory_of`] it (`/dir/index.html`).
fn page_directory(page_url: &url::Url) -> url::Url {
    let last = page_url.path().rsplit('/').next().unwrap_or_default();
    if last.contains('.') {
        return directory_of(page_url);
    }
    let mut dir = page_url.clone();
    dir.set_query(None);
    dir.set_fragment(None);
    if !last.is_empty() {
        let path = format!("{}/", dir.path());
        dir.set_path(&path);
    }
    dir
}

/// The folders to read: links to a directory inside `page_dir`, without
/// their query (listings link their sort orders as `sub/?C=N`), each once.
fn subfolders(page_dir: &url::Url, links: &[url::Url]) -> Vec<url::Url> {
    let mut seen = HashSet::new();
    let mut folders = Vec::new();
    for link in links {
        let mut folder = link.clone();
        folder.set_query(None);
        if folder.path().ends_with('/')
            && is_inside(page_dir, &folder)
            && seen.insert(folder.clone())
        {
            folders.push(folder);
        }
    }
    folders
}

/// `/talks/`: where `folder` is below the page's directory.
fn folder_label(page_dir: &url::Url, folder: &url::Url) -> String {
    let relative = folder
        .path()
        .get(page_dir.path().len()..)
        .unwrap_or_default();
    let relative = percent_encoding::percent_decode_str(relative).decode_utf8_lossy();
    format!("/{relative}")
}

/// A subfolder's final URL, its links and whether its page was cut; `None`
/// if it cannot be read, is not a web page, or redirected out of `page_dir`.
async fn read_folder(
    client: reqwest::Client,
    page_dir: url::Url,
    folder: url::Url,
) -> Option<(url::Url, Vec<url::Url>, bool)> {
    let page = read_page(&client, &folder).await.ok()?;
    if !is_inside(&page_dir, &page.url) {
        return None;
    }
    let (url, cut) = (page.url.clone(), page.cut);
    let links = tokio::task::spawn_blocking(move || extract_links(&page.url, &page.html))
        .await
        .ok()?;
    Some((url, links, cut))
}

async fn read_page(client: &reqwest::Client, url: &url::Url) -> Result<Page, CoreError> {
    match tokio::time::timeout(PAGE_TIMEOUT, fetch(client, url)).await {
        Ok(page) => page,
        Err(_) => Err(CoreError {
            kind: ErrorKind::Timeout,
            detail: "the page took too long to arrive".to_string(),
        }),
    }
}

/// GETs `url`, reading its body only if it is HTML, and no further than
/// [`MAX_PAGE_BYTES`].
async fn fetch(client: &reqwest::Client, url: &url::Url) -> Result<Page, CoreError> {
    let mut response = client
        .get(url.clone())
        .send()
        .await
        .map_err(request_error)?;
    let status = response.status();
    if page::is_challenge_header(response.headers()) {
        return Err(bot_check_error(status));
    }
    if !status.is_success() {
        return Err(status_error(status));
    }
    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty());
    let declared = content_type.is_some();
    if declared && !page::is_html(content_type) {
        return Err(not_a_web_page());
    }
    let final_url = response.url().clone();
    let mut body: Vec<u8> = Vec::new();
    let mut cut = false;
    while let Some(chunk) = response.chunk().await.map_err(request_error)? {
        let room = MAX_PAGE_BYTES - body.len();
        if chunk.len() > room {
            body.extend_from_slice(&chunk[..room]);
            cut = true;
            break;
        }
        body.extend_from_slice(&chunk);
        if body.len() == MAX_PAGE_BYTES {
            // Exactly full: more is only a cut if something follows.
            cut = response.chunk().await.map_err(request_error)?.is_some();
            break;
        }
    }
    if !declared && !sniffs_as_html(&body) {
        return Err(not_a_web_page());
    }
    Ok(Page {
        url: final_url,
        html: String::from_utf8_lossy(&body).into_owned(),
        cut,
    })
}

/// For an answer with no Content-Type: whether the start of the body has
/// the markup of a web page.
fn sniffs_as_html(body: &[u8]) -> bool {
    let head = String::from_utf8_lossy(&body[..body.len().min(1024)]).to_ascii_lowercase();
    [
        "<!doctype html",
        "<html",
        "<head",
        "<body",
        "<title",
        "<h1",
        "<a ",
    ]
    .iter()
    .any(|marker| head.contains(marker))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(s: &str) -> url::Url {
        url::Url::parse(s).unwrap()
    }

    #[test]
    fn directory_of_cases() {
        assert_eq!(
            directory_of(&url("http://h/a/b.html?x=1#f")).as_str(),
            "http://h/a/"
        );
        assert_eq!(directory_of(&url("http://h/a/")).as_str(), "http://h/a/");
        assert_eq!(directory_of(&url("http://h/a")).as_str(), "http://h/");
        assert_eq!(directory_of(&url("http://h")).as_str(), "http://h/");
    }

    #[test]
    fn page_directory_cases() {
        let dir = |s| page_directory(&url(s)).to_string();
        assert_eq!(dir("http://h/d?x=1"), "http://h/d/");
        assert_eq!(dir("http://h/d/"), "http://h/d/");
        assert_eq!(dir("http://h/d/?C=N;O=D"), "http://h/d/");
        assert_eq!(dir("http://h/d/index.html"), "http://h/d/");
        assert_eq!(dir("http://h"), "http://h/");
    }

    #[test]
    fn folders_and_labels() {
        let dir = url("http://h/files/");
        let links = [
            "http://h/files/sub/",
            "http://h/files/sub/?C=N;O=D",
            "http://h/files/a.zip",
            "http://h/files/my%20dir/",
            "http://h/",
            "http://h/files/",
        ]
        .map(url);
        let folders = subfolders(&dir, &links);
        assert_eq!(
            folders.iter().map(url::Url::as_str).collect::<Vec<_>>(),
            ["http://h/files/sub/", "http://h/files/my%20dir/"]
        );
        assert_eq!(folder_label(&dir, &folders[0]), "/sub/");
        assert_eq!(folder_label(&dir, &folders[1]), "/my dir/");
    }

    #[test]
    fn sniffing() {
        assert!(sniffs_as_html(b"\xef\xbb\xbf\n  <!DOCTYPE HTML><p>"));
        assert!(sniffs_as_html(b"<a href=x>y</a>"));
        assert!(!sniffs_as_html(b"PK\x03\x04binary"));
        assert!(!sniffs_as_html(b"{\"a\": 1}"));
        assert!(!sniffs_as_html(b""));
    }

    #[tokio::test]
    async fn a_probe_that_does_not_answer_is_none_after_the_timeout() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        // Accepts and never answers.
        let held = tokio::spawn(async move {
            let mut sockets = Vec::new();
            while let Ok((socket, _)) = listener.accept().await {
                sockets.push(socket);
            }
        });
        let started = std::time::Instant::now();
        let sizes = probe_sizes_with(
            &crate::network::build_client(&crate::network::ProxySetting::System),
            vec![
                format!("http://{addr}/a.bin"),
                format!("http://{addr}/b.bin"),
            ],
            Duration::from_millis(300),
        )
        .await;
        held.abort();
        assert_eq!(sizes, [None, None]);
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn a_streamed_probe_that_does_not_answer_is_none_after_the_timeout() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let held = tokio::spawn(async move {
            let mut sockets = Vec::new();
            while let Ok((socket, _)) = listener.accept().await {
                sockets.push(socket);
            }
        });
        let started = std::time::Instant::now();
        let urls = vec![
            format!("http://{addr}/a.bin"),
            "ftp://example.test/b.bin".to_string(),
            format!("http://{addr}/c.bin"),
        ];
        let mut stream = probe_stream_with(
            crate::network::build_client(&crate::network::ProxySetting::System),
            urls,
            Duration::from_millis(300),
        );
        let mut got = Vec::new();
        while let Some(answer) = stream.recv().await {
            got.push(answer);
        }
        held.abort();
        // The link that cannot be probed answers first.
        assert_eq!(got.first(), Some(&(1, None)));
        got.sort();
        assert_eq!(got, [(0, None), (1, None), (2, None)]);
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn run_bounded_limits_concurrency_and_keeps_order() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let (now, peak) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let (n, p) = (now.clone(), peak.clone());
        let out = run_bounded((0..12u64).collect(), 4, move |i| {
            let (n, p) = (n.clone(), p.clone());
            async move {
                let current = n.fetch_add(1, Ordering::SeqCst) + 1;
                p.fetch_max(current, Ordering::SeqCst);
                // Later items finish first.
                tokio::time::sleep(Duration::from_millis(60 - i * 4)).await;
                n.fetch_sub(1, Ordering::SeqCst);
                i * 10
            }
        })
        .await;
        assert_eq!(out, (0..12u64).map(|i| Some(i * 10)).collect::<Vec<_>>());
        assert_eq!(peak.load(Ordering::SeqCst), 4);
    }
}
