//! HTTP Basic auth for protected downloads: which requests carry a job's
//! user name and password, and what a 401 means.

use reqwest::header::{HeaderMap, WWW_AUTHENTICATE};
use reqwest::{RequestBuilder, Response, StatusCode};

use crate::errors::{CoreError, ErrorKind};
use crate::secrets::Credentials;

/// The detail of a 401 to a request that carried the job's credentials.
pub const WRONG_CREDENTIALS: &str = "wrong user name or password";
/// Longest realm quoted in an error; servers choose it.
const MAX_REALM: usize = 100;

/// A job's credentials and where they are sent: its link's origin (scheme,
/// host, port) and, for an `http` link on the default port, the same host
/// over `https` on the default port (a site upgrading itself). Never a
/// downgrade, another host or another port: not a mirror or confirm form
/// elsewhere.
#[derive(Clone, Debug)]
pub struct Auth {
    credentials: Credentials,
    origin: url::Origin,
}

impl Auth {
    /// `None` if `link` is not a URL.
    pub fn new(credentials: Credentials, link: &str) -> Option<Auth> {
        let origin = url::Url::parse(link).ok()?.origin();
        origin.is_tuple().then_some(Auth {
            credentials,
            origin,
        })
    }

    fn covers(&self, url: &str) -> bool {
        url::Url::parse(url).is_ok_and(|url| {
            let to = url.origin();
            to == self.origin || is_https_upgrade(&self.origin, &to)
        })
    }
}

/// Whether `to` is `from` upgraded to https: `http://host` (port 80) to
/// `https://host` (port 443), same host. Other ports do not pair up: a
/// server's http and https ports cannot be the same, and any other mapping
/// would be a guess.
pub(crate) fn is_https_upgrade(from: &url::Origin, to: &url::Origin) -> bool {
    match (from, to) {
        (
            url::Origin::Tuple(from_scheme, from_host, 80),
            url::Origin::Tuple(to_scheme, to_host, 443),
        ) => from_scheme == "http" && to_scheme == "https" && from_host == to_host,
        _ => false,
    }
}

/// `request` (to `url`) with the credentials when `auth` covers `url`, and
/// whether it got them.
fn authorize(request: RequestBuilder, auth: Option<&Auth>, url: &str) -> (RequestBuilder, bool) {
    match auth.filter(|auth| auth.covers(url)) {
        // `basic_auth` marks the header sensitive: it is never printed.
        Some(auth) => (
            request.basic_auth(&auth.credentials.username, Some(&auth.credentials.password)),
            true,
        ),
        None => (request, false),
    }
}

/// Sends the request `build` makes for `url`, with the credentials where
/// `auth` covers it. reqwest drops them on a redirect that changes the
/// origin (an https upgrade included, or a hop through another host): a 401
/// from a covered URL that a redirect led to is asked again there, once,
/// directly, with the credentials. Returns the answer and whether the
/// request that got it carried them; only an unredirected request is sure
/// to have, so a 401 is "wrong user name or password" only then.
pub(crate) async fn send(
    build: impl Fn(&str) -> RequestBuilder,
    auth: Option<&Auth>,
    url: &str,
) -> reqwest::Result<(Response, bool)> {
    let (request, signed_in) = authorize(build(url), auth, url);
    let response = request.send().await?;
    let direct = url::Url::parse(url).is_ok_and(|u| &u == response.url());
    if response.status() != StatusCode::UNAUTHORIZED || direct {
        return Ok((response, signed_in && direct));
    }
    let landed = response.url().clone();
    match authorize(build(landed.as_str()), auth, landed.as_str()) {
        (request, true) => {
            let again = request.send().await?;
            let sent = again.url() == &landed;
            Ok((again, sent))
        }
        (_, false) => Ok((response, false)),
    }
}

/// The error of a 401 answer: the credentials were wrong if the request
/// carried them; otherwise the server asks for some (naming its realm).
pub(crate) fn unauthorized(headers: &HeaderMap, sent: bool) -> CoreError {
    let detail = if sent {
        WRONG_CREDENTIALS.to_owned()
    } else {
        let realm = headers
            .get_all(WWW_AUTHENTICATE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .find_map(realm);
        match realm {
            Some(realm) => format!("the server asks for a user name and password ({realm:?})"),
            None => "the server asks for a user name and password".to_owned(),
        }
    };
    CoreError {
        kind: ErrorKind::NeedsSignIn,
        detail,
    }
}

/// The `realm` of a `WWW-Authenticate` value, quoted or not, without
/// control characters and cut to `MAX_REALM` characters.
fn realm(challenge: &str) -> Option<String> {
    let at = challenge.to_ascii_lowercase().find("realm=")?;
    let rest = &challenge[at + "realm=".len()..];
    let value = match rest.strip_prefix('"') {
        Some(quoted) => quoted.split('"').next().unwrap_or(""),
        None => rest.split([',', ' ']).next().unwrap_or(""),
    };
    let value: String = value
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_REALM)
        .collect();
    (!value.is_empty()).then_some(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::HeaderValue;

    fn creds() -> Credentials {
        Credentials {
            username: "ann".into(),
            password: "pw".into(),
        }
    }

    #[test]
    fn credentials_go_to_their_origin_only() {
        let auth = Auth::new(creds(), "https://files.example/a/x.iso").unwrap();
        assert!(auth.covers("https://files.example/b/y.iso?t=1"));
        assert!(auth.covers("https://FILES.example:443/x"));
        assert!(!auth.covers("http://files.example/x.iso"));
        assert!(!auth.covers("https://files.example:8443/x.iso"));
        assert!(!auth.covers("https://cdn.files.example/x.iso"));
        assert!(!auth.covers("https://mirror.example/x.iso"));
        assert!(Auth::new(creds(), "not a url").is_none());
        assert!(!format!("{auth:?}").contains("pw"));
    }

    #[test]
    fn an_http_link_covers_its_https_upgrade_only() {
        let auth = Auth::new(creds(), "http://files.example/x.iso").unwrap();
        assert!(auth.covers("https://files.example/x.iso"));
        assert!(auth.covers("https://files.example:443/y"));
        assert!(!auth.covers("https://files.example:8443/x.iso"));
        assert!(!auth.covers("https://cdn.files.example/x.iso"));
        assert!(!auth.covers("https://other.example/x.iso"));
        // Never a downgrade, nor an upgrade from another port.
        let https = Auth::new(creds(), "https://files.example/x.iso").unwrap();
        assert!(!https.covers("http://files.example/x.iso"));
        let port = Auth::new(creds(), "http://files.example:8080/x.iso").unwrap();
        assert!(!port.covers("https://files.example/x.iso"));
        assert!(!port.covers("https://files.example:8080/x.iso"));
    }

    #[test]
    fn https_upgrade_decision() {
        let origin = |s: &str| url::Url::parse(s).unwrap().origin();
        let up = |a: &str, b: &str| is_https_upgrade(&origin(a), &origin(b));
        assert!(up("http://h.test/", "https://h.test/"));
        assert!(up("http://H.test:80/", "https://h.test:443/"));
        assert!(!up("https://h.test/", "http://h.test/"));
        assert!(!up("http://h.test/", "http://h.test/"));
        assert!(!up("https://h.test/", "https://h.test/"));
        assert!(!up("http://h.test/", "https://g.test/"));
        assert!(!up("http://h.test:81/", "https://h.test/"));
        assert!(!up("http://h.test/", "https://h.test:444/"));
        assert!(!up("http://127.0.0.1/", "https://[::1]/"));
        assert!(!up("http://h.test/", "ftp://h.test/"));
    }

    #[test]
    fn authorize_adds_basic_auth_where_covered() {
        let client = reqwest::Client::new();
        let auth = Auth::new(creds(), "http://h.test/x").unwrap();
        let target = "http://h.test/y";
        let (request, sent) = authorize(client.get(target), Some(&auth), target);
        let request = request.build().unwrap();
        assert!(sent);
        // base64("ann:pw")
        assert_eq!(request.headers()["authorization"], "Basic YW5uOnB3");
        assert!(request.headers()["authorization"].is_sensitive());
        let other = "http://other.test/y";
        let (request, sent) = authorize(client.get(other), Some(&auth), other);
        assert!(!sent);
        assert!(
            request
                .build()
                .unwrap()
                .headers()
                .get("authorization")
                .is_none()
        );
        let (_, sent) = authorize(client.get(target), None, target);
        assert!(!sent);
    }

    #[test]
    fn a_401_names_the_realm_or_the_wrong_password() {
        let mut headers = HeaderMap::new();
        headers.append(WWW_AUTHENTICATE, HeaderValue::from_static("Negotiate"));
        headers.append(
            WWW_AUTHENTICATE,
            HeaderValue::from_static("Basic realm=\"Members, only\", charset=\"UTF-8\""),
        );
        let e = unauthorized(&headers, false);
        assert_eq!(e.kind, ErrorKind::NeedsSignIn);
        assert_eq!(
            e.detail,
            "the server asks for a user name and password (\"Members, only\")"
        );
        let e = unauthorized(&headers, true);
        assert_eq!(
            (e.kind, e.detail.as_str()),
            (ErrorKind::NeedsSignIn, WRONG_CREDENTIALS)
        );
        let e = unauthorized(&HeaderMap::new(), false);
        assert_eq!(e.detail, "the server asks for a user name and password");
    }

    #[test]
    fn realm_forms() {
        assert_eq!(realm("Basic realm=\"test\"").as_deref(), Some("test"));
        assert_eq!(realm("Basic REALM=files, x=y").as_deref(), Some("files"));
        assert_eq!(realm("Basic realm=\"\"").as_deref(), None);
        assert_eq!(realm("Bearer").as_deref(), None);
        let long = format!("Basic realm=\"{}\"", "r".repeat(500));
        assert_eq!(realm(&long).map(|r| r.len()), Some(MAX_REALM));
        assert_eq!(realm("Basic realm=\"a\u{7}b\"").as_deref(), Some("ab"));
    }
}
