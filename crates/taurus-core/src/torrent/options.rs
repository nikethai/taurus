//! Options for the librqbit session, and starting it: where it keeps its
//! state, what it binds (IPv6 policy, a taken port) and the rate-limit
//! clamp.

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};
use std::num::NonZeroU32;
use std::path::Path;
use std::sync::Arc;

use librqbit::dht::DhtPersistenceConfig;
use librqbit::limits::LimitsConfig;
use librqbit::{
    DhtSessionConfig, ListenerMode, ListenerOptions, Session, SessionOptions,
    SessionPersistenceConfig,
};

use super::hub::lib_error;
use crate::errors::CoreError;
use crate::settings::TorrentSettings;

/// Under `data_dir`: librqbit's torrent list and fastresume bitfields.
pub(super) const SESSION_DIR: &str = "torrent-session";
/// Under `SESSION_DIR`: the DHT's routing table and port.
pub(super) const DHT_FILE: &str = "dht.json";
/// librqbit moves data in 16 KiB blocks and its limiter's burst is one
/// second of the limit, so a lower limit stalls the transfer.
pub(super) const MIN_BPS: u64 = 16_384;

/// Starts the librqbit session, which restores the torrents it saved. A
/// listening port that is set but cannot be bound (another program has it)
/// falls back to a random one.
pub(super) async fn start_session(
    data_dir: &Path,
    settings: &TorrentSettings,
    test: bool,
    limits: LimitsConfig,
) -> Result<Arc<Session>, CoreError> {
    let ipv6 = ipv6_available();
    let start = |settings: &TorrentSettings| {
        let opts = session_options(data_dir, settings, test, ipv6, limits);
        tracing::info!(
            listen = ?opts.listen.as_ref().map(|l| l.listen_addr),
            ipv6,
            "starting the torrent session"
        );
        Session::new_with_opts(data_dir.join(SESSION_DIR), opts)
    };
    match (start(settings).await, settings.listen_port) {
        (Ok(session), _) => Ok(session),
        // librqbit 9.1's context for a listener that would not bind.
        (Err(e), Some(port)) if format!("{e:#}").contains("error starting listeners") => {
            tracing::warn!("could not listen on port {port} ({e:#}); using a random port");
            let random = TorrentSettings {
                listen_port: None,
                ..settings.clone()
            };
            start(&random).await.map_err(lib_error)
        }
        (Err(e), _) => Err(lib_error(e)),
    }
}

/// Whether this host can open IPv6 sockets at all.
pub(super) fn ipv6_available() -> bool {
    UdpSocket::bind((Ipv6Addr::UNSPECIFIED, 0)).is_ok()
}

/// The session's options. Without IPv6 everything binds IPv4, since
/// librqbit's listener and DHT default to `[::]` and fail to start there.
pub(super) fn session_options(
    data_dir: &Path,
    settings: &TorrentSettings,
    test: bool,
    ipv6: bool,
    ratelimits: LimitsConfig,
) -> SessionOptions {
    let port = settings.listen_port.unwrap_or(0);
    let listen_addr: SocketAddr = if test {
        (Ipv4Addr::LOCALHOST, port).into()
    } else if ipv6 {
        (Ipv6Addr::UNSPECIFIED, port).into()
    } else {
        (Ipv4Addr::UNSPECIFIED, port).into()
    };
    SessionOptions {
        persistence: Some(SessionPersistenceConfig::Json {
            folder: Some(data_dir.join(SESSION_DIR)),
        }),
        fastresume: true,
        listen: Some(ListenerOptions {
            mode: ListenerMode::TcpOnly,
            listen_addr,
            enable_upnp_port_forwarding: settings.upnp && !test,
            ipv4_only: !ipv6,
            ..Default::default()
        }),
        // DHT state goes in our data dir, not librqbit's shared one.
        dht: (settings.dht && !test).then(|| DhtSessionConfig {
            persistence: Some(DhtPersistenceConfig {
                config_filename: Some(data_dir.join(SESSION_DIR).join(DHT_FILE)),
                ..Default::default()
            }),
            ..Default::default()
        }),
        disable_trackers: test,
        disable_local_service_discovery: test,
        ipv4_only: !ipv6,
        peer_limit: settings.peer_limit.map(|n| n as usize),
        client_name_and_version: Some(format!("Hyades/{}", crate::VERSION)),
        ratelimits,
        ..Default::default()
    }
}

pub(super) fn limits_config(download_bps: Option<u64>, upload_bps: Option<u64>) -> LimitsConfig {
    LimitsConfig {
        download_bps: clamp_bps(download_bps),
        upload_bps: clamp_bps(upload_bps),
    }
}

pub(super) fn clamp_bps(bps: Option<u64>) -> Option<NonZeroU32> {
    let bps = bps?.clamp(MIN_BPS, u64::from(u32::MAX));
    NonZeroU32::new(u32::try_from(bps).unwrap_or(u32::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(test: bool, ipv6: bool) -> SessionOptions {
        let settings = TorrentSettings {
            listen_port: Some(6881),
            peer_limit: Some(50),
            ..TorrentSettings::default()
        };
        let limits = limits_config(Some(100_000), None);
        session_options(Path::new("/data"), &settings, test, ipv6, limits)
    }

    #[test]
    fn product_options_with_ipv6() {
        let o = options(false, true);
        let listen = o.listen.as_ref().unwrap();
        assert_eq!(listen.listen_addr, "[::]:6881".parse().unwrap());
        assert!(!listen.ipv4_only && !o.ipv4_only);
        assert!(listen.enable_upnp_port_forwarding);
        assert!(matches!(listen.mode, ListenerMode::TcpOnly));
        let dht = o.dht.as_ref().unwrap().persistence.as_ref().unwrap();
        assert_eq!(
            dht.config_filename.as_deref(),
            Some(Path::new("/data/torrent-session/dht.json"))
        );
        assert!(!o.disable_trackers && !o.disable_local_service_discovery);
        assert!(o.fastresume);
        match &o.persistence {
            Some(SessionPersistenceConfig::Json { folder }) => {
                assert_eq!(folder.as_deref(), Some(Path::new("/data/torrent-session")))
            }
            _ => panic!("JSON persistence expected"),
        }
        assert_eq!(o.peer_limit, Some(50));
        assert_eq!(
            o.client_name_and_version,
            Some(format!("Hyades/{}", env!("CARGO_PKG_VERSION")))
        );
        assert_eq!(o.ratelimits.download_bps, NonZeroU32::new(100_000));
        assert_eq!(o.ratelimits.upload_bps, None);
    }

    #[test]
    fn product_options_without_ipv6_bind_ipv4() {
        let o = options(false, false);
        let listen = o.listen.as_ref().unwrap();
        assert_eq!(listen.listen_addr, "0.0.0.0:6881".parse().unwrap());
        assert!(listen.ipv4_only);
        // The DHT binds 0.0.0.0 too.
        assert!(o.ipv4_only);
        assert!(o.dht.is_some());
    }

    #[test]
    fn random_port_and_settings_off() {
        let settings = TorrentSettings {
            upnp: false,
            dht: false,
            ..TorrentSettings::default()
        };
        let o = session_options(
            Path::new("/d"),
            &settings,
            false,
            true,
            LimitsConfig::default(),
        );
        let listen = o.listen.as_ref().unwrap();
        assert_eq!(listen.listen_addr.port(), 0);
        assert!(!listen.enable_upnp_port_forwarding);
        assert!(o.dht.is_none());
        assert_eq!(o.peer_limit, None);
    }

    #[test]
    fn test_options_are_private_loopback() {
        for ipv6 in [true, false] {
            let o = options(true, ipv6);
            let listen = o.listen.as_ref().unwrap();
            // The port is the one set (else random), on loopback.
            assert_eq!(listen.listen_addr, "127.0.0.1:6881".parse().unwrap());
            assert!(!listen.enable_upnp_port_forwarding);
            assert!(o.dht.is_none());
            assert!(o.disable_trackers && o.disable_local_service_discovery);
            assert!(o.persistence.is_some() && o.fastresume);
        }
    }

    #[test]
    fn limits_are_clamped() {
        let n = NonZeroU32::new;
        assert_eq!(clamp_bps(None), None);
        assert_eq!(clamp_bps(Some(0)), n(16_384));
        assert_eq!(clamp_bps(Some(16_383)), n(16_384));
        assert_eq!(clamp_bps(Some(16_385)), n(16_385));
        assert_eq!(clamp_bps(Some(u64::from(u32::MAX) + 1)), n(u32::MAX));
        assert_eq!(clamp_bps(Some(u64::MAX)), n(u32::MAX));
    }
}
