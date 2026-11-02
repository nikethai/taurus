//! `config get` / `config set`: the engine settings as `key = value` text,
//! pure functions, no I/O.

use std::fmt::Write;

use taurus_core::{ProxySetting, Settings, SpeedLimit, WhenDone};

pub const KEYS: [&str; 15] = [
    "max_active",
    "speed",
    "when_done",
    "auto_checksum",
    "keep_awake",
    "torrent.ratio",
    "torrent.seed_time",
    "torrent.upload_limit",
    "torrent.download_limit",
    "torrent.max_active",
    "torrent.peers",
    "torrent.port",
    "torrent.upnp",
    "torrent.dht",
    "network.proxy",
];

/// The settings that apply only when the torrent session next starts: the
/// peer limit, port, UPnP and DHT.
pub const RESTART_KEYS: [&str; 4] = [
    "torrent.peers",
    "torrent.port",
    "torrent.upnp",
    "torrent.dht",
];
/// Printed after one of those is set.
pub const RESTART_NOTE: &str = "applies next time Taurus starts";

const SPEED_VALUES: &str = "fastest, balanced, gentle, <N>KB/s or <N>MB/s";
const WHEN_DONE_VALUES: &str = "nothing, notify, sleep, shutdown or quit";
const RATE_VALUES: &str = "off, <N>KB/s or <N>MB/s";
const DURATION_VALUES: &str = "off, or a duration such as 30m, 2h or 1d";
const MIN_PORT: u16 = 1024;

/// A `torrent.*` key's field in the settings JSON.
pub fn json_field(key: &str) -> Option<(&'static str, &'static str)> {
    Some(match key {
        "torrent.ratio" => ("torrent", "stop_ratio"),
        "torrent.seed_time" => ("torrent", "stop_after_secs"),
        "torrent.upload_limit" => ("torrent", "upload_limit_bps"),
        "torrent.download_limit" => ("torrent", "download_limit_bps"),
        "torrent.max_active" => ("torrent", "max_active"),
        "torrent.peers" => ("torrent", "peer_limit"),
        "torrent.port" => ("torrent", "listen_port"),
        "torrent.upnp" => ("torrent", "upnp"),
        "torrent.dht" => ("torrent", "dht"),
        "network.proxy" => ("network", "proxy"),
        _ => return None,
    })
}

/// The value of `key` as `config set` takes it.
pub fn get(settings: &Settings, key: &str) -> Result<String, String> {
    Ok(match key {
        "max_active" => settings.max_active.to_string(),
        "speed" => format_speed(&settings.speed),
        "when_done" => format_when_done(settings.when_done).to_string(),
        "auto_checksum" => on_off(settings.auto_checksum).to_string(),
        "keep_awake" => on_off(settings.keep_awake).to_string(),
        "torrent.ratio" => settings
            .torrent
            .stop_ratio
            .map_or("never".to_string(), |r| format!("{r:?}")),
        "torrent.seed_time" => settings
            .torrent
            .stop_after_secs
            .map_or("off".to_string(), format_duration),
        "torrent.upload_limit" => format_limit(settings.torrent.upload_limit_bps),
        "torrent.download_limit" => format_limit(settings.torrent.download_limit_bps),
        "torrent.max_active" => settings.torrent.max_active.to_string(),
        "torrent.peers" => settings
            .torrent
            .peer_limit
            .map_or("default".to_string(), |n| n.to_string()),
        "torrent.port" => settings
            .torrent
            .listen_port
            .map_or("random".to_string(), |p| p.to_string()),
        "torrent.upnp" => on_off(settings.torrent.upnp).to_string(),
        "torrent.dht" => on_off(settings.torrent.dht).to_string(),
        "network.proxy" => format_proxy(&settings.network.proxy),
        _ => return Err(unknown_key(key)),
    })
}

/// Sets `key` from its text. Leaves the settings as they were on error.
pub fn set(settings: &mut Settings, key: &str, value: &str) -> Result<(), String> {
    match key {
        "max_active" => {
            settings.max_active = value
                .trim()
                .parse()
                .map_err(|_| format!("invalid max_active '{value}': use a whole number"))?;
        }
        "speed" => settings.speed = parse_speed(value)?,
        "when_done" => settings.when_done = parse_when_done(value)?,
        "auto_checksum" => settings.auto_checksum = parse_on_off(key, value)?,
        "keep_awake" => settings.keep_awake = parse_on_off(key, value)?,
        "torrent.ratio" => settings.torrent.stop_ratio = parse_ratio(value)?,
        "torrent.seed_time" => settings.torrent.stop_after_secs = parse_duration(value)?,
        "torrent.upload_limit" => settings.torrent.upload_limit_bps = parse_limit(value)?,
        "torrent.download_limit" => settings.torrent.download_limit_bps = parse_limit(value)?,
        "torrent.max_active" => {
            settings.torrent.max_active = value
                .trim()
                .parse()
                .ok()
                .filter(|&n| n >= 1)
                .ok_or_else(|| {
                    format!("invalid {key} '{value}': use a whole number, at least 1")
                })?;
        }
        "torrent.peers" => {
            settings.torrent.peer_limit = match value.trim().to_ascii_lowercase().as_str() {
                "default" => None,
                n => Some(n.parse().ok().filter(|&n| n >= 1).ok_or_else(|| {
                    format!("invalid {key} '{value}': use default, or a whole number, at least 1")
                })?),
            };
        }
        "torrent.port" => {
            settings.torrent.listen_port = match value.trim().to_ascii_lowercase().as_str() {
                "random" => None,
                p => Some(p.parse().ok().filter(|&p| p >= MIN_PORT).ok_or_else(|| {
                    format!(
                        "invalid {key} '{value}': use random, or a port from {MIN_PORT} to 65535"
                    )
                })?),
            };
        }
        "torrent.upnp" => settings.torrent.upnp = parse_on_off(key, value)?,
        "torrent.dht" => settings.torrent.dht = parse_on_off(key, value)?,
        "network.proxy" => settings.network.proxy = parse_proxy_setting(value)?,
        _ => return Err(unknown_key(key)),
    }
    Ok(())
}

/// One `key = value` line per setting.
pub fn lines(settings: &Settings) -> String {
    let mut out = String::new();
    for key in KEYS {
        if let Ok(value) = get(settings, key) {
            let _ = writeln!(out, "{key} = {value}");
        }
    }
    out
}

/// `system`, `none` or a proxy URL (`http://`, `https://`, `socks5://` or
/// `socks5h://`, no user name or password). The engine's own refusal of a
/// URL is the error, as it words it.
fn parse_proxy_setting(value: &str) -> Result<ProxySetting, String> {
    let text = value.trim();
    let setting = match text.to_ascii_lowercase().as_str() {
        "system" => ProxySetting::System,
        "none" => ProxySetting::None,
        _ => ProxySetting::Manual {
            url: text.to_string(),
        },
    };
    setting.check().map_err(|e| e.detail)?;
    Ok(setting)
}

fn format_proxy(proxy: &ProxySetting) -> String {
    match proxy {
        ProxySetting::System => "system".to_string(),
        ProxySetting::None => "none".to_string(),
        ProxySetting::Manual { url } => url.clone(),
    }
}

/// `fastest|balanced|gentle|<N>KB/s|<N>MB/s`, ignoring case; KB = 1000 bytes,
/// MB = 1_000_000 bytes. A custom speed must be above 0.
pub fn parse_speed(value: &str) -> Result<SpeedLimit, String> {
    let invalid = || format!("invalid speed '{value}': use {SPEED_VALUES}");
    let lower = value.trim().to_ascii_lowercase();
    match lower.as_str() {
        "fastest" => return Ok(SpeedLimit::Fastest),
        "balanced" => return Ok(SpeedLimit::Balanced),
        "gentle" => return Ok(SpeedLimit::Gentle),
        _ => {}
    }
    parse_rate(&lower)
        .map(|bytes_per_sec| SpeedLimit::Custom { bytes_per_sec })
        .ok_or_else(invalid)
}

/// `<N>KB/s` or `<N>MB/s` in bytes per second, ignoring case and spaces;
/// above 0.
fn parse_rate(value: &str) -> Option<u64> {
    let lower = value.trim().to_ascii_lowercase();
    let (number, unit) = lower
        .strip_suffix("kb/s")
        .map(|n| (n, 1_000))
        .or_else(|| lower.strip_suffix("mb/s").map(|n| (n, 1_000_000)))?;
    number
        .trim()
        .parse::<u64>()
        .ok()
        .and_then(|n| n.checked_mul(unit))
        .filter(|&bps| bps > 0)
}

/// The `config set` spelling; a custom speed in the largest unit that
/// divides it.
pub fn format_speed(speed: &SpeedLimit) -> String {
    match speed {
        SpeedLimit::Fastest => "fastest".to_string(),
        SpeedLimit::Balanced => "balanced".to_string(),
        SpeedLimit::Gentle => "gentle".to_string(),
        SpeedLimit::Custom { bytes_per_sec } => format_rate(*bytes_per_sec),
    }
}

fn format_rate(bps: u64) -> String {
    if bps.is_multiple_of(1_000_000) {
        format!("{}MB/s", bps / 1_000_000)
    } else if bps.is_multiple_of(1_000) {
        format!("{}KB/s", bps / 1_000)
    } else {
        format!("{bps}B/s")
    }
}

/// `off`, or a rate.
fn parse_limit(value: &str) -> Result<Option<u64>, String> {
    if value.trim().eq_ignore_ascii_case("off") {
        return Ok(None);
    }
    parse_rate(value)
        .map(Some)
        .ok_or_else(|| format!("invalid limit '{value}': use {RATE_VALUES}"))
}

fn format_limit(limit: Option<u64>) -> String {
    limit.map_or("off".to_string(), format_rate)
}

/// A decimal number from 0 up, or `never` (`None`).
pub fn parse_ratio(value: &str) -> Result<Option<f64>, String> {
    let text = value.trim();
    if text.eq_ignore_ascii_case("never") {
        return Ok(None);
    }
    text.parse::<f64>()
        .ok()
        .filter(|r| r.is_finite() && *r >= 0.0)
        .map(Some)
        .ok_or_else(|| format!("invalid ratio '{value}': use never, or a number from 0"))
}

/// `off`, or `<N>s|m|h|d` above 0.
pub fn parse_duration(value: &str) -> Result<Option<u64>, String> {
    let lower = value.trim().to_ascii_lowercase();
    if lower == "off" {
        return Ok(None);
    }
    let invalid = || format!("invalid duration '{value}': use {DURATION_VALUES}");
    let unit: u64 = match lower.chars().last() {
        Some('s') => 1,
        Some('m') => 60,
        Some('h') => 3_600,
        Some('d') => 86_400,
        _ => return Err(invalid()),
    };
    lower[..lower.len() - 1]
        .trim()
        .parse::<u64>()
        .ok()
        .and_then(|n| n.checked_mul(unit))
        .filter(|&secs| secs > 0)
        .map(Some)
        .ok_or_else(invalid)
}

/// The largest unit that divides the seconds: `2h`, `90s`.
pub fn format_duration(secs: u64) -> String {
    for (unit, name) in [(86_400, "d"), (3_600, "h"), (60, "m")] {
        if secs.is_multiple_of(unit) {
            return format!("{}{name}", secs / unit);
        }
    }
    format!("{secs}s")
}

fn parse_when_done(value: &str) -> Result<WhenDone, String> {
    Ok(match value.trim().to_ascii_lowercase().as_str() {
        "nothing" => WhenDone::Nothing,
        "notify" => WhenDone::Notify,
        "sleep" => WhenDone::Sleep,
        "shutdown" => WhenDone::Shutdown,
        "quit" => WhenDone::Quit,
        _ => {
            return Err(format!(
                "invalid when_done '{value}': use {WHEN_DONE_VALUES}"
            ));
        }
    })
}

pub fn format_when_done(action: WhenDone) -> &'static str {
    match action {
        WhenDone::Nothing => "nothing",
        WhenDone::Notify => "notify",
        WhenDone::Sleep => "sleep",
        WhenDone::Shutdown => "shutdown",
        WhenDone::Quit => "quit",
    }
}

fn parse_on_off(key: &str, value: &str) -> Result<bool, String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "on" => Ok(true),
        "off" => Ok(false),
        _ => Err(format!("invalid {key} '{value}': use on or off")),
    }
}

fn on_off(on: bool) -> &'static str {
    if on { "on" } else { "off" }
}

fn unknown_key(key: &str) -> String {
    format!("unknown key '{key}': use {}", KEYS.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEFAULT_TORRENT_LINES: &str = "torrent.ratio = 1.0\ntorrent.seed_time = off\n\
         torrent.upload_limit = off\ntorrent.download_limit = off\ntorrent.max_active = 3\n\
         torrent.peers = default\ntorrent.port = random\ntorrent.upnp = on\ntorrent.dht = on\nnetwork.proxy = system\n";

    fn custom(bytes_per_sec: u64) -> SpeedLimit {
        SpeedLimit::Custom { bytes_per_sec }
    }

    #[test]
    fn speed_grammar() {
        assert_eq!(parse_speed("fastest"), Ok(SpeedLimit::Fastest));
        assert_eq!(parse_speed("Balanced"), Ok(SpeedLimit::Balanced));
        assert_eq!(parse_speed("GENTLE"), Ok(SpeedLimit::Gentle));
        assert_eq!(parse_speed("5MB/s"), Ok(custom(5_000_000)));
        assert_eq!(parse_speed("5mb/s"), Ok(custom(5_000_000)));
        assert_eq!(parse_speed("750KB/s"), Ok(custom(750_000)));
        assert_eq!(parse_speed("750 kB/S"), Ok(custom(750_000)));
        for bad in [
            "fast", "", "5", "5GB/s", "0MB/s", "-1KB/s", "1.5MB/s", "KB/s",
        ] {
            let err = parse_speed(bad).unwrap_err();
            assert!(err.contains(SPEED_VALUES), "{bad}: {err}");
        }
        assert!(parse_speed(&format!("{}MB/s", u64::MAX)).is_err());
    }

    #[test]
    fn speed_formats_in_the_largest_unit() {
        assert_eq!(format_speed(&SpeedLimit::Fastest), "fastest");
        assert_eq!(format_speed(&custom(5_000_000)), "5MB/s");
        assert_eq!(format_speed(&custom(750_000)), "750KB/s");
        assert_eq!(format_speed(&custom(1_500_000)), "1500KB/s");
        assert_eq!(format_speed(&custom(1234)), "1234B/s");
    }

    #[test]
    fn set_then_get_every_key() {
        let mut s = Settings::default();
        set(&mut s, "max_active", "2").unwrap();
        set(&mut s, "speed", "gentle").unwrap();
        set(&mut s, "when_done", "Shutdown").unwrap();
        set(&mut s, "auto_checksum", "off").unwrap();
        set(&mut s, "keep_awake", "OFF").unwrap();
        assert_eq!(
            lines(&s),
            "max_active = 2\nspeed = gentle\nwhen_done = shutdown\n\
             auto_checksum = off\nkeep_awake = off\n"
                .to_string()
                + DEFAULT_TORRENT_LINES
        );
        assert_eq!(get(&s, "when_done").unwrap(), "shutdown");
    }

    #[test]
    fn defaults_as_lines() {
        assert_eq!(
            lines(&Settings::default()),
            "max_active = 3\nspeed = fastest\nwhen_done = nothing\n\
             auto_checksum = on\nkeep_awake = on\n"
                .to_string()
                + DEFAULT_TORRENT_LINES
        );
    }

    #[test]
    fn invalid_values_leave_settings_alone() {
        let mut s = Settings::default();
        assert!(set(&mut s, "max_active", "many").is_err());
        assert!(
            set(&mut s, "when_done", "reboot")
                .unwrap_err()
                .contains(WHEN_DONE_VALUES)
        );
        assert!(
            set(&mut s, "keep_awake", "yes")
                .unwrap_err()
                .contains("on or off")
        );
        assert!(
            set(&mut s, "colour", "red")
                .unwrap_err()
                .contains("max_active")
        );
        assert!(get(&s, "colour").is_err());
        assert_eq!(s, Settings::default());
    }

    #[test]
    fn torrent_keys_roundtrip() {
        let mut s = Settings::default();
        set(&mut s, "torrent.ratio", "0").unwrap();
        assert_eq!(s.torrent.stop_ratio, Some(0.0));
        set(&mut s, "torrent.ratio", "1.5").unwrap();
        assert_eq!(get(&s, "torrent.ratio").unwrap(), "1.5");
        set(&mut s, "torrent.ratio", "Never").unwrap();
        assert_eq!(s.torrent.stop_ratio, None);
        assert_eq!(get(&s, "torrent.ratio").unwrap(), "never");
        set(&mut s, "torrent.seed_time", "2h").unwrap();
        assert_eq!(s.torrent.stop_after_secs, Some(7200));
        set(&mut s, "torrent.seed_time", "90s").unwrap();
        assert_eq!(get(&s, "torrent.seed_time").unwrap(), "90s");
        set(&mut s, "torrent.seed_time", "1d").unwrap();
        assert_eq!(get(&s, "torrent.seed_time").unwrap(), "1d");
        set(&mut s, "torrent.seed_time", "30m").unwrap();
        assert_eq!(get(&s, "torrent.seed_time").unwrap(), "30m");
        set(&mut s, "torrent.seed_time", "off").unwrap();
        assert_eq!(s.torrent.stop_after_secs, None);
        set(&mut s, "torrent.upload_limit", "500 kb/s").unwrap();
        assert_eq!(s.torrent.upload_limit_bps, Some(500_000));
        set(&mut s, "torrent.download_limit", "2MB/s").unwrap();
        assert_eq!(get(&s, "torrent.download_limit").unwrap(), "2MB/s");
        set(&mut s, "torrent.upload_limit", "OFF").unwrap();
        assert_eq!(s.torrent.upload_limit_bps, None);
        set(&mut s, "torrent.max_active", "5").unwrap();
        set(&mut s, "torrent.peers", "80").unwrap();
        assert_eq!(s.torrent.peer_limit, Some(80));
        set(&mut s, "torrent.peers", "default").unwrap();
        assert_eq!(s.torrent.peer_limit, None);
        set(&mut s, "torrent.port", "51413").unwrap();
        assert_eq!(s.torrent.listen_port, Some(51413));
        set(&mut s, "torrent.port", "random").unwrap();
        assert_eq!(s.torrent.listen_port, None);
        set(&mut s, "torrent.upnp", "off").unwrap();
        set(&mut s, "torrent.dht", "off").unwrap();
        assert!(!s.torrent.upnp && !s.torrent.dht);
    }

    #[test]
    fn invalid_torrent_values_leave_settings_alone() {
        let mut s = Settings::default();
        for (key, bad) in [
            ("torrent.ratio", "-1"),
            ("torrent.ratio", "nan"),
            ("torrent.ratio", "inf"),
            ("torrent.ratio", "lots"),
            ("torrent.seed_time", "0m"),
            ("torrent.seed_time", "5"),
            ("torrent.seed_time", "h"),
            ("torrent.seed_time", "-2h"),
            ("torrent.upload_limit", "0KB/s"),
            ("torrent.download_limit", "fast"),
            ("torrent.max_active", "0"),
            ("torrent.max_active", "-1"),
            ("torrent.peers", "0"),
            ("torrent.port", "1023"),
            ("torrent.port", "65536"),
            ("torrent.upnp", "yes"),
            ("torrent.dht", "1"),
        ] {
            assert!(set(&mut s, key, bad).is_err(), "{key} {bad}");
        }
        assert_eq!(s, Settings::default());
    }

    #[test]
    fn restart_keys_are_the_session_settings() {
        for key in [
            "torrent.peers",
            "torrent.port",
            "torrent.upnp",
            "torrent.dht",
        ] {
            assert!(RESTART_KEYS.contains(&key), "{key}");
        }
        assert_eq!(RESTART_KEYS.len(), 4);
    }

    #[test]
    fn durations_format_in_the_largest_unit() {
        assert_eq!(format_duration(7200), "2h");
        assert_eq!(format_duration(5400), "90m");
        assert_eq!(format_duration(86_400 * 3), "3d");
        assert_eq!(format_duration(61), "61s");
    }

    #[test]
    fn proxy_grammar() {
        let mut s = Settings::default();
        assert_eq!(get(&s, "network.proxy").unwrap(), "system");
        set(&mut s, "network.proxy", "http://proxy.example:3128").unwrap();
        assert_eq!(
            get(&s, "network.proxy").unwrap(),
            "http://proxy.example:3128"
        );
        set(&mut s, "network.proxy", " NONE ").unwrap();
        assert_eq!(s.network.proxy, ProxySetting::None);
        set(&mut s, "network.proxy", "System").unwrap();
        assert_eq!(s.network.proxy, ProxySetting::System);
        set(&mut s, "network.proxy", "socks5h://127.0.0.1:9050").unwrap();
        let before = s.clone();
        for bad in ["", "garbage", "ftp://h:1", "http://u:p@h:1", "http://u@h:1"] {
            let e = set(&mut s, "network.proxy", bad).unwrap_err();
            assert!(e.starts_with("unsupported proxy"), "{bad}: {e}");
            assert!(!e.contains("http://u"), "{bad}: {e}");
            assert_eq!(s, before, "{bad}");
        }
        assert!(lines(&s).ends_with("network.proxy = socks5h://127.0.0.1:9050\n"));
    }
}
