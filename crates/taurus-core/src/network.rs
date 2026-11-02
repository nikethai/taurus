//! The proxy setting: checking it, the HTTP client built from it, and what
//! yt-dlp and the torrent session are given for it.
//!
//! Every web request of the engine (downloads and their probes, grab, tool
//! installs, thumbnails) goes through one [`SharedClient`], rebuilt when the
//! setting changes; yt-dlp gets `--proxy`; the torrent session uses a SOCKS
//! proxy only, from its next start.

use std::sync::{Arc, PoisonError, RwLock};
use std::time::Duration;

use reqwest::header::{ACCEPT_ENCODING, HeaderMap, HeaderValue};
use url::Url;

use crate::errors::{CoreError, ErrorKind};
use crate::http::client::USER_AGENT;

/// The port of a SOCKS proxy URL that names none.
const SOCKS_PORT: u16 = 1080;

/// How the engine reaches the web.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ProxySetting {
    /// The OS proxy settings (on Linux, the `HTTP_PROXY`/`HTTPS_PROXY`/
    /// `NO_PROXY` environment variables).
    #[default]
    System,
    /// Direct connections, whatever the OS says.
    None,
    /// Everything through this proxy: `http://`, `https://`, `socks5://` or
    /// `socks5h://` (names resolved by the proxy), with an optional
    /// `user:password@`.
    Manual { url: String },
}

impl ProxySetting {
    /// Fails, as [`parse_proxy`] does, for a manual proxy that cannot be
    /// used (or that the HTTP client refuses).
    pub fn check(&self) -> Result<(), CoreError> {
        match self {
            ProxySetting::Manual { url } => manual_proxy(url).map(drop),
            ProxySetting::System | ProxySetting::None => Ok(()),
        }
    }
}

/// The proxy URL, if it is one the engine can use: scheme `http`, `https`,
/// `socks5` or `socks5h`, and a host. Otherwise an `Unknown` error whose
/// detail starts with "unsupported proxy" and never repeats the URL (it may
/// hold a password).
pub fn parse_proxy(url: &str) -> Result<Url, CoreError> {
    let unsupported = |why: &str| CoreError {
        kind: ErrorKind::Unknown,
        detail: format!("unsupported proxy: {why}"),
    };
    let parsed = Url::parse(url)
        .map_err(|_| unsupported("not a URL such as http://host:port or socks5h://host:port"))?;
    if !matches!(parsed.scheme(), "http" | "https" | "socks5" | "socks5h") {
        return Err(unsupported(
            "only http, https, socks5 and socks5h proxies are supported",
        ));
    }
    if parsed.host_str().is_none_or(str::is_empty) {
        return Err(unsupported("the URL names no host"));
    }
    Ok(parsed)
}

/// The client every web request uses: stable User-Agent, no content
/// encoding (byte offsets must match the file), no automatic Referer, and
/// `proxy`. A manual proxy that [`parse_proxy`] refuses (never one the
/// engine accepted) is ignored with a warning: the system settings apply.
pub fn build_client(proxy: &ProxySetting) -> reqwest::Client {
    let mut headers = HeaderMap::new();
    headers.insert(ACCEPT_ENCODING, HeaderValue::from_static("identity"));
    let builder = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .default_headers(headers)
        .connect_timeout(Duration::from_secs(20))
        .read_timeout(Duration::from_secs(60))
        .redirect(reqwest::redirect::Policy::limited(10))
        // A redirect keeps the job's Referer (as browsers do) instead of
        // naming the redirecting URL.
        .referer(false);
    let builder = match proxy {
        ProxySetting::System => builder,
        ProxySetting::None => builder.no_proxy(),
        ProxySetting::Manual { url } => match manual_proxy(url) {
            // Every request, local addresses included.
            Ok(proxy) => builder.proxy(proxy),
            Err(e) => {
                tracing::warn!("ignoring the manual proxy setting: {e}");
                builder
            }
        },
    };
    builder
        .build()
        .expect("static HTTP client configuration is valid")
}

fn manual_proxy(url: &str) -> Result<reqwest::Proxy, CoreError> {
    let url = parse_proxy(url)?;
    reqwest::Proxy::all(url.as_str()).map_err(|_| CoreError {
        kind: ErrorKind::Unknown,
        detail: "unsupported proxy: the HTTP client refused it".to_string(),
    })
}

/// yt-dlp's arguments for `proxy`: `--proxy <url>` for a manual one,
/// `--proxy ""` (direct) for None, nothing for System (yt-dlp then reads
/// the environment like the engine does).
pub fn ytdlp_proxy_args(proxy: &ProxySetting) -> Vec<String> {
    match proxy {
        ProxySetting::System => Vec::new(),
        ProxySetting::None => vec!["--proxy".to_string(), String::new()],
        ProxySetting::Manual { url } => vec!["--proxy".to_string(), url.clone()],
    }
}

/// The torrent session's proxy for `proxy`: only a manual `socks5` or
/// `socks5h` one (librqbit has no HTTP proxy for peers), with port 1080
/// when it names none. librqbit's peer connections and HTTP trackers go
/// through it; its UDP trackers and DHT do not.
pub fn torrent_proxy(proxy: &ProxySetting) -> Option<String> {
    let ProxySetting::Manual { url } = proxy else {
        return None;
    };
    let mut url = parse_proxy(url).ok()?;
    if !matches!(url.scheme(), "socks5" | "socks5h") {
        return None;
    }
    if url.port().is_none() {
        url.set_port(Some(SOCKS_PORT)).ok()?;
    }
    Some(url.to_string())
}

/// Whether going from `old` to `new` changes the torrent session's proxy,
/// which a running session only takes when the engine next opens (a front
/// end then says so).
pub fn torrent_proxy_changes(old: &ProxySetting, new: &ProxySetting) -> bool {
    torrent_proxy(old) != torrent_proxy(new)
}

/// The engine's HTTP client, shared by everything that makes web requests
/// and swapped for a new one when the proxy changes. Holders keep the
/// handle and take the client ([`SharedClient::get`]) for each request, so
/// a new proxy applies to every request from then on; requests already
/// sent finish as they started.
#[derive(Clone)]
pub struct SharedClient(Arc<RwLock<reqwest::Client>>);

impl SharedClient {
    pub fn new(proxy: &ProxySetting) -> SharedClient {
        SharedClient::from(build_client(proxy))
    }

    /// The current client (a cheap handle onto its connection pool).
    pub fn get(&self) -> reqwest::Client {
        self.0
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Replaces the client with one for `proxy`, for every holder.
    pub fn set_proxy(&self, proxy: &ProxySetting) {
        let client = build_client(proxy);
        *self.0.write().unwrap_or_else(PoisonError::into_inner) = client;
    }
}

impl From<reqwest::Client> for SharedClient {
    fn from(client: reqwest::Client) -> SharedClient {
        SharedClient(Arc::new(RwLock::new(client)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manual(url: &str) -> ProxySetting {
        ProxySetting::Manual {
            url: url.to_owned(),
        }
    }

    #[test]
    fn parse_proxy_cases() {
        for good in [
            "http://127.0.0.1:8080",
            "https://proxy.example:443",
            "socks5://10.0.0.1:1080",
            "socks5h://user:secret@proxy.example:1080",
            "http://proxy.example",
        ] {
            let url = parse_proxy(good).unwrap_or_else(|e| panic!("{good}: {e}"));
            assert!(url.host_str().is_some(), "{good}");
        }
        for bad in [
            "ftp://proxy.example:21",
            "localhost:8080",
            "proxy.example:8080",
            "nope",
            "",
            "http://",
            "socks5://",
            "socks5h://:1080",
            "socks4://proxy.example:1080",
        ] {
            let e = parse_proxy(bad).expect_err(bad);
            assert_eq!(e.kind, ErrorKind::Unknown, "{bad}");
            assert!(e.detail.starts_with("unsupported proxy"), "{bad}: {e}");
        }
        // A refused URL's password is never repeated.
        let e = parse_proxy("ftp://user:hunter2@proxy.example").unwrap_err();
        assert!(!e.detail.contains("hunter2"), "{e}");
        assert!(manual("nope").check().is_err());
        assert!(ProxySetting::System.check().is_ok() && ProxySetting::None.check().is_ok());
    }

    #[test]
    fn system_and_none_args() {
        assert!(ytdlp_proxy_args(&ProxySetting::System).is_empty());
        assert_eq!(ytdlp_proxy_args(&ProxySetting::None), ["--proxy", ""]);
        assert_eq!(
            ytdlp_proxy_args(&manual("socks5h://127.0.0.1:9050")),
            ["--proxy", "socks5h://127.0.0.1:9050"]
        );
    }

    #[test]
    fn torrent_proxy_only_socks() {
        assert_eq!(torrent_proxy(&ProxySetting::System), None);
        assert_eq!(torrent_proxy(&ProxySetting::None), None);
        assert_eq!(torrent_proxy(&manual("http://127.0.0.1:8080")), None);
        assert_eq!(torrent_proxy(&manual("https://proxy.example")), None);
        assert_eq!(
            torrent_proxy(&manual("socks5://127.0.0.1:9050")).as_deref(),
            Some("socks5://127.0.0.1:9050")
        );
        assert_eq!(
            torrent_proxy(&manual("socks5h://u:p@proxy.example:1081")).as_deref(),
            Some("socks5h://u:p@proxy.example:1081")
        );
        // librqbit needs a port.
        assert_eq!(
            torrent_proxy(&manual("socks5h://proxy.example")).as_deref(),
            Some("socks5h://proxy.example:1080")
        );
        assert_eq!(torrent_proxy(&manual("socks5://")), None);
        assert!(torrent_proxy_changes(
            &ProxySetting::System,
            &manual("socks5://127.0.0.1:9050")
        ));
        assert!(!torrent_proxy_changes(
            &ProxySetting::None,
            &manual("http://127.0.0.1:8080")
        ));
    }

    #[test]
    fn proxy_setting_json() {
        assert_eq!(ProxySetting::default(), ProxySetting::System);
        let json = serde_json::to_string(&manual("http://h:1")).unwrap();
        assert_eq!(json, r#"{"Manual":{"url":"http://h:1"}}"#);
        assert_eq!(
            serde_json::from_str::<ProxySetting>(r#""None""#).unwrap(),
            ProxySetting::None
        );
    }

    #[tokio::test]
    async fn every_setting_builds_a_client() {
        for proxy in [
            ProxySetting::System,
            ProxySetting::None,
            manual("http://127.0.0.1:9"),
            manual("https://127.0.0.1:9"),
            manual("socks5://127.0.0.1:9"),
            manual("socks5h://u:p@127.0.0.1:9"),
            // Refused by `set_settings`; ignored here.
            manual("nope"),
        ] {
            let _ = build_client(&proxy);
        }
    }
}
