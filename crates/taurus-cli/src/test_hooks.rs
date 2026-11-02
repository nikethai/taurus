//! Switches for the CLI's own integration tests, read from the environment
//! ONLY in debug builds (`cfg(debug_assertions)`). The integration tests run
//! the debug binary, so they keep working; a release build, which is what
//! users get, never reads these variables. They are not user features.
//!
//! - `TAURUS_FAST_TIMING=1`: short retry and when-done delays
//!   (`Timing::fast_for_tests`), so the tests do not wait in real time.
//! - `TAURUS_TORRENT_TEST_PEERS=host:port,...`: the peers every torrent
//!   gets; the torrent session leaves DHT, trackers, LSD and UPnP off.
//! - `TAURUS_TOOL_SOURCES=<base url>`: yt-dlp, ffmpeg and Deno come from
//!   the fake host at this URL.
//! - `TAURUS_SECRETS_IN_MEMORY=1`: passwords are kept in memory, not in the
//!   OS keychain, so a test never touches the real one.
//! - `TAURUS_PASSWORD=<text>`: the password `--user` and `login` read,
//!   instead of asking on the terminal (see [`password`]). A release build
//!   never reads it, so a password never comes from the environment there.

use taurus_core::EngineConfig;

/// Applies the switches set in the environment; nothing in a release build.
pub(crate) fn apply(config: &mut EngineConfig) {
    #[cfg(debug_assertions)]
    apply_from(config, |name| std::env::var_os(name));
    #[cfg(not(debug_assertions))]
    let _ = config;
}

/// The password the test environment gives; always `None` in a release
/// build.
pub(crate) fn password() -> Option<String> {
    #[cfg(debug_assertions)]
    return std::env::var("TAURUS_PASSWORD").ok();
    #[cfg(not(debug_assertions))]
    None
}

/// `apply`, with the variables read through `var`.
#[cfg(any(debug_assertions, test))]
fn apply_from(config: &mut EngineConfig, var: impl Fn(&str) -> Option<std::ffi::OsString>) {
    if var("TAURUS_FAST_TIMING").is_some_and(|v| v == "1") {
        config.timing = taurus_core::Timing::fast_for_tests();
    }
    if let Some(peers) = var("TAURUS_TORRENT_TEST_PEERS") {
        config.torrent_test = Some(taurus_core::torrent::hub::TorrentTestNet {
            initial_peers: peers
                .to_string_lossy()
                .split(',')
                .filter_map(|peer| peer.trim().parse::<std::net::SocketAddr>().ok())
                .collect(),
            restore_in_place: false,
        });
    }
    if var("TAURUS_SECRETS_IN_MEMORY").is_some_and(|v| v == "1") {
        config.secrets = std::sync::Arc::new(taurus_core::secrets::MemoryStore::new());
    }
    if let Some(base) = var("TAURUS_TOOL_SOURCES") {
        match url::Url::parse(&base.to_string_lossy()) {
            Ok(base) => config.tool_sources = taurus_core::tools::ToolSources::under(&base),
            Err(e) => eprintln!("warning: ignoring TAURUS_TOOL_SOURCES: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn the_switches_set_the_config() {
        let mut config = EngineConfig::new(PathBuf::from("/data"));
        let default_wait = config.timing.waiting_retry_first;
        let default_secrets = config.secrets.clone();
        apply_from(&mut config, |name| match name {
            "TAURUS_FAST_TIMING" => Some("1".into()),
            "TAURUS_TORRENT_TEST_PEERS" => Some("127.0.0.1:6881, junk,[::1]:7".into()),
            "TAURUS_TOOL_SOURCES" => Some("http://127.0.0.1:9/".into()),
            "TAURUS_SECRETS_IN_MEMORY" => Some("1".into()),
            _ => None,
        });
        assert!(config.timing.waiting_retry_first < default_wait);
        // Passwords are in memory, not in the default (keychain) store.
        assert!(!std::sync::Arc::ptr_eq(&config.secrets, &default_secrets));
        let peers = config.torrent_test.unwrap().initial_peers;
        assert_eq!(peers.len(), 2);
        assert_eq!(
            config.tool_sources.yt_dlp.as_str(),
            "http://127.0.0.1:9/yt-dlp/"
        );
        // Unset, nothing changes.
        let mut config = EngineConfig::new(PathBuf::from("/data"));
        apply_from(&mut config, |_| None);
        assert!(config.torrent_test.is_none());
        assert_eq!(config.timing.waiting_retry_first, default_wait);
        let default_secrets = config.secrets.clone();
        apply_from(&mut config, |_| None);
        assert!(std::sync::Arc::ptr_eq(&config.secrets, &default_secrets));
    }
}
