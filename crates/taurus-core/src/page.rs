//! Recognizes an HTML answer where a file was expected: a bot check, a quota
//! page, a confirm form to follow, a captive portal or an ordinary web page.
//! Bot checks are only detected, never solved.

use reqwest::header::HeaderMap;

/// The most HTML the engine reads from one response.
pub const MAX_HTML_BYTES: usize = 65_536;

#[derive(Clone, Debug, PartialEq)]
pub enum PageVerdict {
    /// -> `ErrorKind::BlockedByBotCheck`
    BotCheck,
    /// -> `ErrorKind::QuotaExceeded`
    QuotaExceeded,
    /// "Download anyway" form; the URL is the request it submits.
    ConfirmForm(url::Url),
    /// -> `ErrorKind::CaptivePortal`
    CaptivePortal,
    /// -> `ErrorKind::SiteNotSupported`, detail "this link is a web page"
    WebPage,
}

/// Last path segment extensions of files that are never served as HTML.
const BINARY_EXTENSIONS: [&str; 28] = [
    "iso", "img", "zip", "7z", "rar", "gz", "xz", "bz2", "zst", "tar", "exe", "msi", "dmg", "pkg",
    "deb", "rpm", "appimage", "apk", "mp4", "mkv", "mov", "avi", "webm", "mp3", "flac", "m4a",
    "pdf", "bin",
];

/// `text/html` or `application/xhtml+xml`, ignoring parameters and case.
pub fn is_html(content_type: Option<&str>) -> bool {
    content_type.is_some_and(|ct| {
        let mime = ct.split(';').next().unwrap_or_default().trim();
        mime.eq_ignore_ascii_case("text/html") || mime.eq_ignore_ascii_case("application/xhtml+xml")
    })
}

/// A Cloudflare challenge, at any status: [`is_challenge_header`], or a
/// Cloudflare server whose page shows a challenge.
pub fn is_bot_check(_status: u16, headers: &HeaderMap, body: &str) -> bool {
    if is_challenge_header(headers) {
        return true;
    }
    let cloudflare = headers
        .get("server")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.to_ascii_lowercase().contains("cloudflare"));
    cloudflare && (body.contains("Just a moment") || body.contains("challenge-platform"))
}

/// `cf-mitigated: challenge`: the bot check visible without reading a body,
/// and the only one a segment response is judged by.
pub fn is_challenge_header(headers: &HeaderMap) -> bool {
    headers
        .get("cf-mitigated")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.trim().eq_ignore_ascii_case("challenge"))
}

/// Judges an HTML answer to a request for `requested`. Rules 1-2 (bot check,
/// quota) apply to every status; the rest assume a 2xx answer, so any other
/// status ends as `WebPage` and the caller maps the status itself.
pub fn classify(requested: &url::Url, status: u16, headers: &HeaderMap, body: &str) -> PageVerdict {
    classify_at(requested, requested, status, headers, body)
}

/// [`classify`] for a page that redirects moved to `page_url`: a confirm
/// form's action is relative to the page, not to the requested URL.
pub(crate) fn classify_at(
    requested: &url::Url,
    page_url: &url::Url,
    status: u16,
    headers: &HeaderMap,
    body: &str,
) -> PageVerdict {
    if is_bot_check(status, headers, body) {
        return PageVerdict::BotCheck;
    }
    if is_quota_page(body) {
        return PageVerdict::QuotaExceeded;
    }
    if !(200..300).contains(&status) {
        return PageVerdict::WebPage;
    }
    if let Some(url) = confirm_form(page_url, body) {
        return PageVerdict::ConfirmForm(url);
    }
    if names_binary_file(requested) {
        return PageVerdict::CaptivePortal;
    }
    PageVerdict::WebPage
}

fn is_quota_page(body: &str) -> bool {
    let body = body.to_lowercase();
    body.contains("too many users have viewed or downloaded this file recently")
        || (body.contains("quota") && body.contains("exceeded"))
}

/// The URL submitted by the page's only form, if that form is a GET with a
/// hidden `confirm` input. Its query is every named input of the form.
fn confirm_form(page_url: &url::Url, body: &str) -> Option<url::Url> {
    let forms = scraper::Selector::parse("form").expect("static selector");
    let inputs = scraper::Selector::parse("input").expect("static selector");
    let doc = scraper::Html::parse_document(body);
    let mut found = doc.select(&forms);
    let form = found.next()?;
    if found.next().is_some() {
        return None;
    }
    if form
        .attr("method")
        .is_some_and(|m| !m.trim().eq_ignore_ascii_case("get"))
    {
        return None;
    }
    let has_confirm = form.select(&inputs).any(|input| {
        input.attr("name") == Some("confirm")
            && input
                .attr("type")
                .is_some_and(|t| t.trim().eq_ignore_ascii_case("hidden"))
    });
    if !has_confirm {
        return None;
    }
    let mut url = page_url
        .join(form.attr("action").unwrap_or_default())
        .ok()?;
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    url.set_fragment(None);
    url.set_query(None);
    {
        let mut query = url.query_pairs_mut();
        for input in form.select(&inputs) {
            if let Some(name) = input.attr("name") {
                query.append_pair(name, input.attr("value").unwrap_or_default());
            }
        }
    }
    Some(url)
}

/// Whether the URL's last path segment ends with a binary file extension.
fn names_binary_file(url: &url::Url) -> bool {
    let last = url
        .path_segments()
        .and_then(|mut segments| segments.next_back())
        .unwrap_or_default()
        .to_ascii_lowercase();
    last.rsplit_once('.')
        .is_some_and(|(_, ext)| BINARY_EXTENSIONS.contains(&ext))
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::{HeaderName, HeaderValue};

    fn headers(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
        pairs
            .iter()
            .map(|&(n, v)| (HeaderName::from_static(n), HeaderValue::from_static(v)))
            .collect()
    }

    fn html() -> HeaderMap {
        headers(&[("content-type", "text/html; charset=utf-8")])
    }

    fn url(s: &str) -> url::Url {
        url::Url::parse(s).unwrap()
    }

    #[test]
    fn html_content_types() {
        assert!(is_html(Some("text/html")));
        assert!(is_html(Some("Text/HTML; charset=UTF-8")));
        assert!(is_html(Some("application/xhtml+xml")));
        assert!(!is_html(Some("application/octet-stream")));
        assert!(!is_html(Some("text/plain")));
        assert!(!is_html(None));
    }

    #[test]
    fn cloudflare_header_is_bot_check() {
        let h = headers(&[("cf-mitigated", "challenge")]);
        assert!(is_bot_check(403, &h, ""));
        assert!(is_bot_check(200, &h, "<html></html>"));
    }

    #[test]
    fn cloudflare_body_is_bot_check() {
        let h = headers(&[("server", "cloudflare"), ("content-type", "text/html")]);
        let body = "<html><title>Just a moment...</title></html>";
        assert!(is_bot_check(403, &h, body));
        assert!(is_bot_check(
            503,
            &h,
            "<script src=\"/cdn-cgi/challenge-platform/x.js\"></script>"
        ));
        assert_eq!(
            classify(&url("https://h.example/a.iso"), 403, &h, body),
            PageVerdict::BotCheck
        );
    }

    #[test]
    fn plain_403_is_not_bot_check() {
        let body = "<html><title>Just a moment...</title></html>";
        assert!(!is_bot_check(403, &html(), body));
        let h = headers(&[("server", "cloudflare"), ("content-type", "text/html")]);
        assert!(!is_bot_check(403, &h, "<html>Forbidden</html>"));
    }

    #[test]
    fn only_the_challenge_header_counts_without_a_body() {
        // Behind Cloudflare every answer says `server: cloudflare`, including
        // the origin's own 403/503 pages.
        let h = headers(&[("server", "cloudflare"), ("content-type", "text/html")]);
        assert!(!is_challenge_header(&h));
        assert!(!is_bot_check(403, &h, ""));
        assert!(!is_bot_check(503, &h, ""));
        assert!(is_challenge_header(&headers(&[(
            "cf-mitigated",
            "Challenge"
        )])));
        assert!(!is_challenge_header(&headers(&[("cf-mitigated", "none")])));
    }

    #[test]
    fn quota_page_detected() {
        let body = "<html><body><p>Sorry, you can't view or download this file at this time.</p>\
            <p>Too many users have viewed or downloaded this file recently. Please try \
            accessing the file again later.</p></body></html>";
        assert_eq!(
            classify(&url("https://h.example/uc?id=1"), 200, &html(), body),
            PageVerdict::QuotaExceeded
        );
        let body = "<html>Download QUOTA for this file has been Exceeded</html>";
        assert_eq!(
            classify(&url("https://h.example/a.iso"), 403, &html(), body),
            PageVerdict::QuotaExceeded
        );
    }

    #[test]
    fn confirm_form_builds_url() {
        let body = r#"<html><body><form action="/download" method="get">
            <input type="hidden" name="id" value="X">
            <input type="hidden" name="confirm" value="t">
            <input type="hidden" name="uuid" value="u">
            </form></body></html>"#;
        assert_eq!(
            classify(&url("https://h.example/view"), 200, &html(), body),
            PageVerdict::ConfirmForm(url("https://h.example/download?id=X&confirm=t&uuid=u"))
        );
    }

    #[test]
    fn confirm_form_needs_one_get_form_with_hidden_confirm() {
        let page = url("https://h.example/view");
        let form = |attrs: &str, inputs: &str| format!("<form {attrs}>{inputs}</form>");
        let confirm = r#"<input type="hidden" name="confirm" value="t">"#;
        // No method means GET; the action's own query is replaced.
        let body = form(r#"action="/dl?old=1""#, confirm);
        assert_eq!(
            classify(&page, 200, &html(), &body),
            PageVerdict::ConfirmForm(url("https://h.example/dl?confirm=t"))
        );
        let post = form(r#"action="/dl" method="post""#, confirm);
        let visible = form(
            r#"action="/dl""#,
            r#"<input type="text" name="confirm" value="t">"#,
        );
        let two = format!(
            "{}{}",
            form(r#"action="/a""#, confirm),
            form(r#"action="/b""#, confirm)
        );
        let scripted = form(r#"action="javascript:go()""#, confirm);
        for body in [post, visible, two, scripted] {
            assert_eq!(
                classify(&page, 200, &html(), &body),
                PageVerdict::WebPage,
                "{body}"
            );
        }
    }

    #[test]
    fn binary_url_html_is_captive_portal() {
        let body = "<html><body>Welcome to Hotel Wi-Fi</body></html>";
        assert_eq!(
            classify(&url("https://h.example/files/a.iso"), 200, &html(), body),
            PageVerdict::CaptivePortal
        );
        assert_eq!(
            classify(
                &url("https://h.example/apps/Tool.AppImage?x=1"),
                200,
                &html(),
                body
            ),
            PageVerdict::CaptivePortal
        );
    }

    #[test]
    fn page_url_html_is_web_page() {
        let body = "<html><body>Index of /files</body></html>";
        assert_eq!(
            classify(&url("https://h.example/files/"), 200, &html(), body),
            PageVerdict::WebPage
        );
        assert_eq!(
            classify(&url("https://h.example/about.html"), 200, &html(), body),
            PageVerdict::WebPage
        );
    }

    #[test]
    fn error_status_page_is_only_bot_check_or_quota() {
        // Rules 3-4 apply to 2xx answers only.
        let body = r#"<form action="/dl"><input type="hidden" name="confirm" value="t"></form>"#;
        assert_eq!(
            classify(&url("https://h.example/a.iso"), 404, &html(), body),
            PageVerdict::WebPage
        );
    }
}
