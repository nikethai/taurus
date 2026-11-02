//! [`TestServer`] as a plain HTTP proxy.
//!
//! Its router matches the request's path, so a request in absolute form
//! (`GET http://any.host/path`, as clients send to a proxy) is answered with
//! what is served at `path`, whatever the host. A middleware records those
//! requests. There is no `CONNECT`: HTTPS cannot go through it.

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::State;
use axum::http::Response;

use crate::TestServer;

/// The absolute-form request targets received, in order.
pub(crate) type Proxied = Arc<Mutex<Vec<String>>>;

impl TestServer {
    /// A server to use as a plain HTTP proxy (its URL is
    /// [`TestServer::base_url`]): absolute-form requests are served by
    /// their path and recorded ([`TestServer::proxied_requests`]).
    /// Requests in the usual form are served too.
    pub async fn start_proxy() -> TestServer {
        // Every server does this; the name says what the test means.
        TestServer::start().await
    }

    /// The full URLs of the absolute-form requests received (the ones sent
    /// to a proxy), oldest first, whatever path they named.
    pub fn proxied_requests(&self) -> Vec<String> {
        self.proxied
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

/// Records the target of an absolute-form request (one with a scheme).
pub(crate) async fn record_proxied(
    State(proxied): State<Proxied>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response<Body> {
    if request.uri().scheme().is_some() {
        proxied
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(request.uri().to_string());
    }
    next.run(request).await
}
