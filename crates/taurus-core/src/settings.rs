//! User settings and the values derived from them.

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

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Settings {
    pub max_active: u32,
    pub speed: SpeedLimit,
    pub when_done: WhenDone,
    pub auto_checksum: bool,
    pub keep_awake: bool,
}

impl Default for Settings {
    fn default() -> Settings {
        Settings {
            max_active: 3,
            speed: SpeedLimit::Fastest,
            when_done: WhenDone::Nothing,
            auto_checksum: true,
            keep_awake: true,
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
    fn settings_default_and_partial_json() {
        let d = Settings::default();
        assert_eq!(d.max_active, 3);
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
