//! `Engine::grab` and `Engine::probe_sizes` against a local web server.

use taurus_core::{Engine, EngineConfig, ErrorKind, GrabOptions};
use taurus_testkit::{FileSpec, TestServer};

async fn engine(tmp: &tempfile::TempDir) -> Engine {
    Engine::open(EngineConfig::new(tmp.path().join("data")))
        .await
        .unwrap()
}

const SUBFOLDERS: GrabOptions = GrabOptions { subfolders: true };
const NO_SUBFOLDERS: GrabOptions = GrabOptions { subfolders: false };

fn base_of(url: &str) -> String {
    url::Url::parse(url).unwrap().origin().ascii_serialization()
}

fn listing(title: &str, links: &[&str]) -> String {
    let rows: String = links
        .iter()
        .map(|l| format!("<a href=\"{l}\">{l}</a>\n"))
        .collect();
    format!(
        "<html><head><title>{title}</title></head><body><h1>{title}</h1><pre>{rows}</pre></body></html>"
    )
}

fn names(result: &taurus_core::GrabResult) -> Vec<(String, String)> {
    result
        .links
        .iter()
        .map(|l| (l.folder.clone(), l.name.clone()))
        .collect()
}

#[tokio::test]
async fn grab_page_lists_files() {
    let tmp = tempfile::tempdir().unwrap();
    let engine = engine(&tmp).await;
    let server = TestServer::start().await;
    let html = r#"<html><body>
        <a href="a.zip">a</a> <a href="docs/b%20c.PDF#p2">b</a> <img src="pic.png">
        <a href="about.html">about</a> <a href="/contact.php">contact</a> <a href="a.zip">again</a>
        </body></html>"#;
    let url = server.serve_page("/p/index.html", 200, &[], html);
    let result = engine.grab(&url, NO_SUBFOLDERS).await.unwrap();
    assert_eq!(result.page_url, url);
    assert_eq!(result.subfolders_scanned, 0);
    assert!(!result.truncated);
    assert_eq!(result.links.len(), 3);
    assert_eq!(result.links[0].url, format!("{}/p/a.zip", base_of(&url)));
    assert_eq!(result.links[0].folder, "/");
    assert_eq!(result.links[0].extension.as_deref(), Some("zip"));
    assert_eq!(result.links[1].name, "b c.PDF");
    assert_eq!(result.links[1].extension.as_deref(), Some("pdf"));
    assert_eq!(result.links[2].name, "pic.png");
}

#[tokio::test]
async fn subfolders_stay_inside() {
    let tmp = tempfile::tempdir().unwrap();
    let engine = engine(&tmp).await;
    let server = TestServer::start().await;
    let page = server.serve_page(
        "/files/",
        200,
        &[],
        &listing(
            "Index of /files/",
            &[
                "../",
                "a.zip",
                "sub/",
                "../up/",
                "/",
                "http://other.host/",
                "/up/",
                "?C=N;O=D",
                "sub/?C=M;O=A",
            ],
        ),
    );
    server.serve_page(
        "/files/sub/",
        200,
        &[],
        &listing(
            "Index of /files/sub/",
            &[
                "../",
                "b.zip",
                "deeper/",
                "../a.zip",
                "http://other.host/x.zip",
            ],
        ),
    );
    server.serve_page("/up/", 200, &[], &listing("Index of /up/", &["z.zip"]));
    server.serve_page(
        "/files/sub/deeper/",
        200,
        &[],
        &listing("Index of /", &["d.zip"]),
    );
    let result = engine.grab(&page, SUBFOLDERS).await.unwrap();
    assert_eq!(result.subfolders_scanned, 1);
    assert_eq!(
        names(&result),
        [
            ("/".to_string(), "a.zip".to_string()),
            ("/sub/".to_string(), "b.zip".to_string()),
        ]
    );
    // Nothing outside the folder, and nothing below the first level, was fetched.
    assert_eq!(server.requests("/up/"), 0);
    assert_eq!(server.requests("/files/sub/deeper/"), 0);
    assert_eq!(server.requests("/files/sub/"), 1);
}

#[tokio::test]
async fn subfolders_need_a_listing_and_the_option() {
    let tmp = tempfile::tempdir().unwrap();
    let engine = engine(&tmp).await;
    let server = TestServer::start().await;
    let plain = server.serve_page(
        "/site/",
        200,
        &[],
        "<title>Home</title><a href=\"sub/\">sub</a><a href=\"a.zip\">a</a>",
    );
    let index = server.serve_page("/pub/", 200, &[], &listing("Index of /pub/", &["sub/"]));
    server.serve_page(
        "/site/sub/",
        200,
        &[],
        &listing("Index of /site/sub/", &["b.zip"]),
    );
    server.serve_page(
        "/pub/sub/",
        200,
        &[],
        &listing("Index of /pub/sub/", &["b.zip"]),
    );
    // A page that is not a listing is not crawled, even when asked.
    let result = engine.grab(&plain, SUBFOLDERS).await.unwrap();
    assert_eq!((result.links.len(), result.subfolders_scanned), (1, 0));
    // A listing is not crawled unless asked.
    let result = engine.grab(&index, NO_SUBFOLDERS).await.unwrap();
    assert_eq!((result.links.len(), result.subfolders_scanned), (0, 0));
    assert_eq!(
        server.requests("/site/sub/") + server.requests("/pub/sub/"),
        0
    );
}

#[tokio::test]
async fn a_subfolder_that_fails_is_skipped() {
    let tmp = tempfile::tempdir().unwrap();
    let engine = engine(&tmp).await;
    let server = TestServer::start().await;
    let page = server.serve_page(
        "/f/",
        200,
        &[],
        &listing("Index of /f/", &["gone/", "ok/", "bin/"]),
    );
    server.serve_page("/f/gone/", 404, &[], "<html>not found</html>");
    server.serve_page("/f/ok/", 200, &[], &listing("Index of /f/ok/", &["x.zip"]));
    server.serve_bytes("/f/bin/", "application/octet-stream", b"\0\0");
    let result = engine.grab(&page, SUBFOLDERS).await.unwrap();
    assert_eq!(result.subfolders_scanned, 1);
    assert_eq!(names(&result), [("/ok/".to_string(), "x.zip".to_string())]);
}

#[tokio::test]
async fn base_href_is_respected() {
    let tmp = tempfile::tempdir().unwrap();
    let engine = engine(&tmp).await;
    let server = TestServer::start().await;
    let url = server.serve_page(
        "/p/page.html",
        200,
        &[],
        r#"<head><base href="/mirror/"></head><a href="a.zip">a</a>"#,
    );
    let result = engine.grab(&url, NO_SUBFOLDERS).await.unwrap();
    assert_eq!(
        result.links[0].url,
        format!("{}/mirror/a.zip", base_of(&url))
    );
}

#[tokio::test]
async fn grab_rejects_non_html() {
    let tmp = tempfile::tempdir().unwrap();
    let engine = engine(&tmp).await;
    let server = TestServer::start().await;
    let file = server.serve("/a.zip", FileSpec::new(1000));
    let text = server.serve_bytes("/t.txt", "text/plain", b"<a href=\"a.zip\">a</a>");
    let json = server.serve_bytes("/j", "application/json", b"{}");
    // No Content-Type: the start of the body decides.
    let sniffed_bin = server.serve_bytes("/nt1", "", b"PK\x03\x04\0\0binary");
    let sniffed_html = server.serve_bytes("/nt2", "", b"\n <!DOCTYPE html><a href=\"x.zip\">x</a>");
    for url in [&file, &text, &json, &sniffed_bin] {
        let e = engine.grab(url, NO_SUBFOLDERS).await.unwrap_err();
        assert_eq!(e.kind, ErrorKind::SiteNotSupported, "{url}");
        assert_eq!(e.detail, "not a web page", "{url}");
    }
    let result = engine.grab(&sniffed_html, NO_SUBFOLDERS).await.unwrap();
    assert_eq!(result.links.len(), 1);
}

#[tokio::test]
async fn grab_reports_http_and_bad_input_errors() {
    let tmp = tempfile::tempdir().unwrap();
    let engine = engine(&tmp).await;
    let server = TestServer::start().await;
    let gone = server.serve_page("/gone", 404, &[], "<html>nope</html>");
    let e = engine.grab(&gone, NO_SUBFOLDERS).await.unwrap_err();
    assert_eq!(e.kind, ErrorKind::LinkExpired);
    let e = engine.grab("not a url", NO_SUBFOLDERS).await.unwrap_err();
    assert_eq!(e.kind, ErrorKind::Unknown);
    let e = engine
        .grab("magnet:?xt=urn:btih:abc", NO_SUBFOLDERS)
        .await
        .unwrap_err();
    assert_eq!(e.kind, ErrorKind::Unknown);
    let e = engine
        .grab("http://127.0.0.1:1/", NO_SUBFOLDERS)
        .await
        .unwrap_err();
    assert_eq!(e.kind, ErrorKind::ConnectionLost);
}

#[tokio::test]
async fn truncates_at_500() {
    let tmp = tempfile::tempdir().unwrap();
    let engine = engine(&tmp).await;
    let server = TestServer::start().await;
    let many: String = (0..600)
        .map(|i| format!("<a href=\"f{i}.bin\">f</a>"))
        .collect();
    let url = server.serve_page("/many/", 200, &[], &many);
    let result = engine.grab(&url, NO_SUBFOLDERS).await.unwrap();
    assert_eq!(result.links.len(), 500);
    assert!(result.truncated);
    assert_eq!(result.links[499].name, "f499.bin");
    let exact: String = (0..500)
        .map(|i| format!("<a href=\"f{i}.bin\">f</a>"))
        .collect();
    let url = server.serve_page("/exact/", 200, &[], &exact);
    let result = engine.grab(&url, NO_SUBFOLDERS).await.unwrap();
    assert_eq!(result.links.len(), 500);
    assert!(!result.truncated);
}

#[tokio::test]
async fn subfolders_count_toward_the_500() {
    let tmp = tempfile::tempdir().unwrap();
    let engine = engine(&tmp).await;
    let server = TestServer::start().await;
    let subs: Vec<String> = (0..10).map(|i| format!("s{i}/")).collect();
    let refs: Vec<&str> = subs.iter().map(String::as_str).collect();
    let page = server.serve_page("/big/", 200, &[], &listing("Index of /big/", &refs));
    let files: Vec<String> = (0..200).map(|i| format!("f{i}.bin")).collect();
    let file_refs: Vec<&str> = files.iter().map(String::as_str).collect();
    for s in &subs {
        server.serve_page(
            &format!("/big/{s}"),
            200,
            &[],
            &listing("Index of x", &file_refs),
        );
    }
    let result = engine.grab(&page, SUBFOLDERS).await.unwrap();
    assert_eq!(result.links.len(), 500);
    assert!(result.truncated);
    assert!(result.subfolders_scanned < 10);
}

#[tokio::test]
async fn a_page_over_5_mib_is_cut_and_flagged() {
    let tmp = tempfile::tempdir().unwrap();
    let engine = engine(&tmp).await;
    let server = TestServer::start().await;
    let mut html = String::from("<a href=\"first.zip\">a</a>");
    html.push_str(&"<!-- padding -->".repeat(5 * 1024 * 1024 / 16));
    html.push_str("<a href=\"last.zip\">a</a>");
    let url = server.serve_page("/huge", 200, &[], &html);
    let result = engine.grab(&url, NO_SUBFOLDERS).await.unwrap();
    assert!(result.truncated);
    assert_eq!(result.links.len(), 1);
    assert_eq!(result.links[0].name, "first.zip");
}

#[tokio::test]
async fn probe_sizes_in_order() {
    let tmp = tempfile::tempdir().unwrap();
    let engine = engine(&tmp).await;
    let server = TestServer::start().await;
    let a = server.serve("/a.bin", FileSpec::new(1234));
    let b = server.serve("/b.bin", FileSpec::new(7));
    let missing = format!("{}/missing.bin", base_of(&a));
    let sizes = engine
        .probe_sizes(vec![a, missing, b, "not a url".to_string()])
        .await;
    assert_eq!(sizes, [Some(1234), None, Some(7), None]);
    assert!(engine.probe_sizes(Vec::new()).await.is_empty());
}
