//! `config get` / `config set`: the engine settings as `key = value` text,
//! pure functions, no I/O.

use std::fmt::Write;

use taurus_core::{Settings, SpeedLimit, WhenDone};

pub const KEYS: [&str; 5] = [
    "max_active",
    "speed",
    "when_done",
    "auto_checksum",
    "keep_awake",
];

const SPEED_VALUES: &str = "fastest, balanced, gentle, <N>KB/s or <N>MB/s";
const WHEN_DONE_VALUES: &str = "nothing, notify, sleep, shutdown or quit";

/// The value of `key` as `config set` takes it.
pub fn get(settings: &Settings, key: &str) -> Result<String, String> {
    Ok(match key {
        "max_active" => settings.max_active.to_string(),
        "speed" => format_speed(&settings.speed),
        "when_done" => format_when_done(settings.when_done).to_string(),
        "auto_checksum" => on_off(settings.auto_checksum).to_string(),
        "keep_awake" => on_off(settings.keep_awake).to_string(),
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
    let (number, unit) = if let Some(n) = lower.strip_suffix("kb/s") {
        (n, 1_000)
    } else if let Some(n) = lower.strip_suffix("mb/s") {
        (n, 1_000_000)
    } else {
        return Err(invalid());
    };
    let bytes_per_sec = number
        .trim()
        .parse::<u64>()
        .ok()
        .and_then(|n| n.checked_mul(unit))
        .filter(|&bps| bps > 0)
        .ok_or_else(invalid)?;
    Ok(SpeedLimit::Custom { bytes_per_sec })
}

/// The `config set` spelling; a custom speed in the largest unit that
/// divides it.
pub fn format_speed(speed: &SpeedLimit) -> String {
    match speed {
        SpeedLimit::Fastest => "fastest".to_string(),
        SpeedLimit::Balanced => "balanced".to_string(),
        SpeedLimit::Gentle => "gentle".to_string(),
        SpeedLimit::Custom { bytes_per_sec: bps } if bps % 1_000_000 == 0 => {
            format!("{}MB/s", bps / 1_000_000)
        }
        SpeedLimit::Custom { bytes_per_sec: bps } if bps % 1_000 == 0 => {
            format!("{}KB/s", bps / 1_000)
        }
        SpeedLimit::Custom { bytes_per_sec: bps } => format!("{bps}B/s"),
    }
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
        );
        assert_eq!(get(&s, "when_done").unwrap(), "shutdown");
    }

    #[test]
    fn defaults_as_lines() {
        assert_eq!(
            lines(&Settings::default()),
            "max_active = 3\nspeed = fastest\nwhen_done = nothing\n\
             auto_checksum = on\nkeep_awake = on\n"
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
}
