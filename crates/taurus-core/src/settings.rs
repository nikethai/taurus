//! User settings and the values derived from them.

pub use crate::network::ProxySetting;

/// Without a measured peak: Gentle = 1 MiB/s, Balanced = 5 MiB/s.
const GENTLE_FALLBACK_BPS: u64 = 1_048_576;
const BALANCED_FALLBACK_BPS: u64 = 5_242_880;

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum SpeedLimit {
    Fastest,
    Balanced,
    Gentle,
    Custom { bytes_per_sec: u64 },
}

impl SpeedLimit {
    /// The cap in bytes per second; `None` is unlimited. Gentle and Balanced
    /// are 25 % and 60 % of the measured peak, when there is one.
    pub fn bytes_per_sec(&self, measured_peak_bps: Option<u64>) -> Option<u64> {
        let share = |percent: u64, fallback: u64| {
            measured_peak_bps.map_or(fallback, |peak| peak.saturating_mul(percent) / 100)
        };
        match self {
            Self::Fastest => None,
            Self::Balanced => Some(share(60, BALANCED_FALLBACK_BPS)),
            Self::Gentle => Some(share(25, GENTLE_FALLBACK_BPS)),
            Self::Custom { bytes_per_sec } => Some(*bytes_per_sec),
        }
    }
}

/// What to do once the queue has nothing left to run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum WhenDone {
    Nothing,
    Notify,
    Sleep,
    Shutdown,
    Quit,
}

/// Torrent-only settings. `None` limits are unlimited (or, for `peer_limit`,
/// the library's default).
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct TorrentSettings {
    /// Stop sharing at this uploaded/size ratio; `Some(0.0)` stops as soon as
    /// the download finishes, `None` never stops.
    pub stop_ratio: Option<f64>,
    /// Stop sharing this long after the download finished.
    pub stop_after_secs: Option<u64>,
    pub upload_limit_bps: Option<u64>,
    /// Caps the torrent side of the download speed.
    pub download_limit_bps: Option<u64>,
    /// Torrents downloading at once; seeding ones do not count.
    pub max_active: u32,
    pub peer_limit: Option<u32>,
    /// `None` picks a random port. Applies when the session next starts.
    pub listen_port: Option<u16>,
    /// Applies when the session next starts.
    pub upnp: bool,
    /// Applies when the session next starts.
    pub dht: bool,
}

impl Default for TorrentSettings {
    fn default() -> TorrentSettings {
        TorrentSettings {
            stop_ratio: Some(1.0),
            stop_after_secs: None,
            upload_limit_bps: None,
            download_limit_bps: None,
            max_active: 3,
            peer_limit: None,
            listen_port: None,
            upnp: true,
            dht: true,
        }
    }
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Settings {
    pub max_active: u32,
    pub speed: SpeedLimit,
    pub when_done: WhenDone,
    pub auto_checksum: bool,
    pub keep_awake: bool,
    /// Connections per file for an add that names none, 1 to 16
    /// (`plan::CONNECTIONS_RANGE`).
    pub connections: u32,
    pub torrent: TorrentSettings,
    pub network: NetworkSettings,
    /// What only the desktop app reads.
    pub ui: UiSettings,
}

impl Default for Settings {
    fn default() -> Settings {
        Settings {
            max_active: 3,
            speed: SpeedLimit::Fastest,
            when_done: WhenDone::Nothing,
            auto_checksum: true,
            keep_awake: true,
            connections: crate::plan::DEFAULT_CONNECTIONS,
            torrent: TorrentSettings::default(),
            network: NetworkSettings::default(),
            ui: UiSettings::default(),
        }
    }
}

/// How the engine reaches the web.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct NetworkSettings {
    /// Applies to every web request from the next one on, and to yt-dlp's
    /// next run; to torrents (a SOCKS proxy only) when the engine next
    /// opens. See [`crate::network`].
    pub proxy: ProxySetting,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum UiMode {
    Simple,
    Advanced,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ThemeChoice {
    System,
    Light,
    Dark,
}

/// The desktop app's own preferences, kept with the engine's settings so
/// there is one place to save them. The engine itself ignores them.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct UiSettings {
    pub mode: UiMode,
    pub theme: ThemeChoice,
    pub accent: String,
    /// Colour a job's progress bar by the kind of file.
    pub progress_by_type: bool,
    pub offer_copied_links: bool,
    pub open_at_login: bool,
    /// Videos, Music and Documents subfolders, chosen by the UI when adding.
    pub sort_by_type: bool,
    /// `None` is the OS language.
    pub language: Option<String>,
    /// Where downloads go; `None` is the OS Downloads folder, which the UI
    /// resolves.
    pub folder: Option<std::path::PathBuf>,
}

impl Default for UiSettings {
    fn default() -> UiSettings {
        UiSettings {
            mode: UiMode::Simple,
            theme: ThemeChoice::System,
            accent: "Aldebaran".to_string(),
            progress_by_type: true,
            offer_copied_links: true,
            open_at_login: false,
            sort_by_type: true,
            language: None,
            folder: None,
        }
    }
}

/// Whether actions that touch the OS (sleep, shutdown) really run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PowerMode {
    Execute,
    DryRun,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn speed_presets() {
        let peak = Some(40_000_000);
        assert_eq!(SpeedLimit::Gentle.bytes_per_sec(peak), Some(10_000_000));
        assert_eq!(SpeedLimit::Balanced.bytes_per_sec(peak), Some(24_000_000));
        assert_eq!(SpeedLimit::Gentle.bytes_per_sec(None), Some(1_048_576));
        assert_eq!(SpeedLimit::Balanced.bytes_per_sec(None), Some(5_242_880));
        assert_eq!(SpeedLimit::Fastest.bytes_per_sec(peak), None);
        assert_eq!(SpeedLimit::Fastest.bytes_per_sec(None), None);
        let custom = SpeedLimit::Custom { bytes_per_sec: 123 };
        assert_eq!(custom.bytes_per_sec(peak), Some(123));
        // Huge peaks saturate instead of overflowing.
        assert!(SpeedLimit::Balanced.bytes_per_sec(Some(u64::MAX)).is_some());
    }

    #[test]
    fn torrent_settings_defaults_and_partial_json() {
        let d = TorrentSettings::default();
        assert_eq!(d.stop_ratio, Some(1.0));
        assert_eq!(d.stop_after_secs, None);
        assert_eq!((d.upload_limit_bps, d.download_limit_bps), (None, None));
        assert_eq!(d.max_active, 3);
        assert_eq!((d.peer_limit, d.listen_port), (None, None));
        assert!(d.upnp && d.dht);
        assert_eq!(Settings::default().torrent, d);

        let s: Settings = serde_json::from_str(r#"{"torrent":{"stop_ratio":null}}"#).unwrap();
        assert_eq!(s.torrent.stop_ratio, None);
        assert_eq!(
            s.torrent,
            TorrentSettings {
                stop_ratio: None,
                ..TorrentSettings::default()
            }
        );
        // Settings saved before M3 have no torrent section.
        let old: Settings = serde_json::from_str(r#"{"max_active":5}"#).unwrap();
        assert_eq!(old.torrent, d);
    }

    #[test]
    fn ui_settings_defaults() {
        let d = UiSettings::default();
        assert_eq!(d.mode, UiMode::Simple);
        assert_eq!(d.theme, ThemeChoice::System);
        assert_eq!(d.accent, "Aldebaran");
        assert!(d.progress_by_type && d.offer_copied_links && d.sort_by_type);
        assert!(!d.open_at_login);
        assert_eq!((d.language, d.folder), (None, None));
        assert_eq!(Settings::default().ui, UiSettings::default());

        let s: Settings =
            serde_json::from_str(r#"{"ui":{"theme":"Dark","folder":"/dl","language":"fr"}}"#)
                .unwrap();
        assert_eq!(
            s.ui,
            UiSettings {
                theme: ThemeChoice::Dark,
                folder: Some("/dl".into()),
                language: Some("fr".to_string()),
                ..UiSettings::default()
            }
        );
    }

    #[test]
    fn old_settings_json_without_ui_loads() {
        // As M3 saved it: everything but `ui`.
        let m3 = r#"{"max_active":5,"speed":"Fastest","when_done":"Nothing","auto_checksum":true,
            "keep_awake":true,"torrent":{"stop_ratio":1.0,"stop_after_secs":null,
            "upload_limit_bps":null,"download_limit_bps":null,"max_active":3,"peer_limit":null,
            "listen_port":null,"upnp":true,"dht":true}}"#;
        let s: Settings = serde_json::from_str(m3).unwrap();
        assert_eq!(s.max_active, 5);
        assert_eq!(s.ui, UiSettings::default());
    }

    #[test]
    fn settings_default_and_partial_json() {
        let d = Settings::default();
        assert_eq!(d.max_active, 3);
        assert_eq!(d.connections, 16);
        assert_eq!(d.speed, SpeedLimit::Fastest);
        assert_eq!(d.when_done, WhenDone::Nothing);
        assert!(d.auto_checksum);
        assert!(d.keep_awake);

        let s: Settings = serde_json::from_str(r#"{"max_active":5}"#).unwrap();
        assert_eq!(
            s,
            Settings {
                max_active: 5,
                ..Settings::default()
            }
        );

        let json = serde_json::to_string(&Settings {
            speed: SpeedLimit::Custom { bytes_per_sec: 9 },
            when_done: WhenDone::Quit,
            ..Settings::default()
        })
        .unwrap();
        let back: Settings = serde_json::from_str(&json).unwrap();
        assert_eq!(back.speed, SpeedLimit::Custom { bytes_per_sec: 9 });
        assert_eq!(back.when_done, WhenDone::Quit);
    }
}
