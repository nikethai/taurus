//! Saved settings, the measured peak speed and learned per-host connection limits.

use rusqlite::{OptionalExtension, params};

use crate::errors::{CoreError, ErrorKind};
use crate::settings::Settings;

use super::{Store, db_err, now_secs};

const SETTINGS_KEY: &str = "settings";
/// Where `load_settings_lenient` keeps saved settings it could not read.
const UNREADABLE_SETTINGS_KEY: &str = "settings.unreadable";
const MEASURED_PEAK_KEY: &str = "measured_peak";

/// A learned connection limit older than this is forgotten: hosts change.
const HOST_CAP_MAX_AGE_SECS: i64 = 7 * 24 * 60 * 60;

impl Store {
    /// The saved settings, or the defaults when none were saved. Unreadable
    /// saved settings are an error, never silently reset.
    pub fn load_settings(&self) -> Result<Settings, CoreError> {
        match self.setting(SETTINGS_KEY)? {
            None => Ok(Settings::default()),
            Some(json) => serde_json::from_str(&json).map_err(|e| CoreError {
                kind: ErrorKind::Unknown,
                detail: format!("saved settings are unreadable: {e}"),
            }),
        }
    }

    /// The saved settings, never failing on their content: unreadable ones
    /// are logged and copied to `settings.unreadable` (replacing an older
    /// copy), then each readable field is kept and the rest defaulted
    /// (everything, if the value is not a JSON object). `max_active` is at
    /// least 1. Only a database error is returned.
    pub fn load_settings_lenient(&self) -> Result<Settings, CoreError> {
        let Some(raw) = self.setting(SETTINGS_KEY)? else {
            return Ok(Settings::default());
        };
        let mut settings = match serde_json::from_str(&raw) {
            Ok(settings) => settings,
            Err(e) => {
                tracing::warn!("saved settings are unreadable ({e}); keeping what can be read");
                self.put_setting(UNREADABLE_SETTINGS_KEY, &raw)?;
                readable_fields(&raw)
            }
        };
        settings.max_active = settings.max_active.max(1);
        Ok(settings)
    }

    pub fn save_settings(&self, s: &Settings) -> Result<(), CoreError> {
        let json = serde_json::to_string(s).map_err(|e| CoreError {
            kind: ErrorKind::Unknown,
            detail: e.to_string(),
        })?;
        self.put_setting(SETTINGS_KEY, &json)
    }

    /// The best download speed seen so far (bytes/s) and when (unix secs).
    pub fn measured_peak(&self) -> Result<Option<(u64, i64)>, CoreError> {
        let Some(raw) = self.setting(MEASURED_PEAK_KEY)? else {
            return Ok(None);
        };
        let parsed = raw
            .split_once(',')
            .and_then(|(bps, at)| Some((bps.parse().ok()?, at.parse().ok()?)));
        parsed.map(Some).ok_or_else(|| CoreError {
            kind: ErrorKind::Unknown,
            detail: format!("saved measured peak {raw:?} is unreadable"),
        })
    }

    pub fn save_measured_peak(&self, bps: u64, at: i64) -> Result<(), CoreError> {
        self.put_setting(MEASURED_PEAK_KEY, &format!("{bps},{at}"))
    }

    /// The learned connection limit of `host` (`host[:port]`), if one was
    /// learned in the last 7 days.
    pub fn host_cap(&self, host: &str) -> Result<Option<u32>, CoreError> {
        let cap: Option<i64> = self
            .lock()
            .query_row(
                "SELECT max_connections FROM host_flags WHERE host = ?1 AND updated_at >= ?2",
                params![host, now_secs().saturating_sub(HOST_CAP_MAX_AGE_SECS)],
                |row| row.get(0),
            )
            .optional()
            .map_err(db_err)?;
        cap.map(|c| {
            u32::try_from(c).map_err(|_| CoreError {
                kind: ErrorKind::Unknown,
                detail: format!("connection limit {c} of {host} is damaged"),
            })
        })
        .transpose()
    }

    /// Records `cap` for `host` unless a lower one was learned in the last 7
    /// days, in one statement, so concurrent saves never raise it; refreshes
    /// its age.
    pub fn set_host_cap(&self, host: &str, cap: u32) -> Result<(), CoreError> {
        self.set_host_cap_at(host, cap, now_secs())
    }

    fn set_host_cap_at(&self, host: &str, cap: u32, at: i64) -> Result<(), CoreError> {
        self.lock()
            .execute(
                "INSERT INTO host_flags (host, max_connections, updated_at) \
                 VALUES (?1, ?2, ?3) ON CONFLICT(host) DO UPDATE SET \
                 max_connections = CASE WHEN updated_at < ?4 THEN excluded.max_connections \
                 ELSE MIN(max_connections, excluded.max_connections) END, \
                 updated_at = excluded.updated_at",
                params![host, cap, at, at.saturating_sub(HOST_CAP_MAX_AGE_SECS)],
            )
            .map_err(db_err)?;
        Ok(())
    }

    fn setting(&self, key: &str) -> Result<Option<String>, CoreError> {
        self.lock()
            .query_row("SELECT value FROM settings WHERE key = ?1", [key], |row| {
                row.get(0)
            })
            .optional()
            .map_err(db_err)
    }

    fn put_setting(&self, key: &str, value: &str) -> Result<(), CoreError> {
        self.lock()
            .execute(
                "INSERT INTO settings (key, value) VALUES (?1, ?2) \
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![key, value],
            )
            .map_err(db_err)?;
        Ok(())
    }
}

/// The defaults, with each field of the JSON object `raw` that reads on its
/// own put in place of its default.
fn readable_fields(raw: &str) -> Settings {
    let Ok(serde_json::Value::Object(saved)) = serde_json::from_str(raw) else {
        return Settings::default();
    };
    let Ok(serde_json::Value::Object(mut fields)) = serde_json::to_value(Settings::default())
    else {
        return Settings::default();
    };
    let reads = |fields: &serde_json::Map<String, serde_json::Value>| {
        serde_json::from_value::<Settings>(serde_json::Value::Object(fields.clone())).is_ok()
    };
    for (key, value) in saved {
        if !fields.contains_key(&key) {
            continue;
        }
        let mut tried = fields.clone();
        tried.insert(key.clone(), value.clone());
        if reads(&tried) {
            fields.insert(key, value);
        } else if let (
            Some(serde_json::Value::Object(mut section)),
            serde_json::Value::Object(saved),
        ) = (fields.get(&key).cloned(), value)
        {
            // A section (`torrent`) keeps its readable fields too.
            for (name, value) in saved {
                if !section.contains_key(&name) {
                    continue;
                }
                let before = section.insert(name.clone(), value);
                tried.insert(key.clone(), serde_json::Value::Object(section.clone()));
                if !reads(&tried) {
                    section.extend(before.map(|v| (name, v)));
                }
            }
            fields.insert(key, serde_json::Value::Object(section));
        }
    }
    serde_json::from_value(serde_json::Value::Object(fields)).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{SpeedLimit, TorrentSettings, WhenDone};
    use crate::store::test_support::open_temp;

    #[test]
    fn lenient_settings_keep_readable_torrent_fields() {
        let (_dir, store) = open_temp();
        let saved = r#"{"max_active":4,"torrent":{"stop_ratio":2.5,"max_active":"many","dht":false,"bogus":1}}"#;
        store.put_setting(SETTINGS_KEY, saved).unwrap();
        let s = store.load_settings_lenient().unwrap();
        assert_eq!(s.max_active, 4);
        assert_eq!(
            s.torrent,
            TorrentSettings {
                stop_ratio: Some(2.5),
                dht: false,
                ..TorrentSettings::default()
            }
        );
        // A torrent section that is not an object falls back to the defaults.
        store
            .put_setting(SETTINGS_KEY, r#"{"max_active":4,"torrent":7}"#)
            .unwrap();
        let s = store.load_settings_lenient().unwrap();
        assert_eq!((s.max_active, s.torrent), (4, TorrentSettings::default()));
    }

    #[test]
    fn settings_roundtrip() {
        let (_dir, store) = open_temp();
        assert_eq!(store.load_settings().unwrap(), Settings::default());

        let s = Settings {
            max_active: 7,
            speed: SpeedLimit::Custom { bytes_per_sec: 42 },
            when_done: WhenDone::Shutdown,
            auto_checksum: false,
            keep_awake: false,
            torrent: TorrentSettings {
                stop_ratio: None,
                max_active: 2,
                listen_port: Some(51413),
                ..TorrentSettings::default()
            },
        };
        store.save_settings(&s).unwrap();
        assert_eq!(store.load_settings().unwrap(), s);
        let s2 = Settings { max_active: 1, ..s };
        store.save_settings(&s2).unwrap();
        assert_eq!(store.load_settings().unwrap(), s2);

        assert_eq!(store.measured_peak().unwrap(), None);
        store.save_measured_peak(40_000_000, 1_700_000_000).unwrap();
        assert_eq!(
            store.measured_peak().unwrap(),
            Some((40_000_000, 1_700_000_000))
        );
        store.save_measured_peak(5, 6).unwrap();
        assert_eq!(store.measured_peak().unwrap(), Some((5, 6)));
    }

    #[test]
    fn malformed_settings_are_an_error_not_a_reset() {
        let (_dir, store) = open_temp();
        {
            let conn = store.conn.lock().unwrap();
            conn.execute(
                "INSERT INTO settings (key, value) VALUES ('settings', 'not json')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO settings (key, value) VALUES ('measured_peak', 'garbage')",
                [],
            )
            .unwrap();
        }
        assert_eq!(store.load_settings().unwrap_err().kind, ErrorKind::Unknown);
        assert_eq!(store.measured_peak().unwrap_err().kind, ErrorKind::Unknown);
    }

    #[test]
    fn host_cap_roundtrip() {
        let (_dir, store) = open_temp();
        assert_eq!(store.host_cap("example.test").unwrap(), None);
        store.set_host_cap("example.test", 2).unwrap();
        store.set_host_cap("other.test:8080", 5).unwrap();
        assert_eq!(store.host_cap("example.test").unwrap(), Some(2));
        store.set_host_cap("example.test", 1).unwrap();
        assert_eq!(store.host_cap("example.test").unwrap(), Some(1));
        assert_eq!(store.host_cap("other.test:8080").unwrap(), Some(5));
    }

    #[test]
    fn host_cap_is_never_raised() {
        let (_dir, store) = open_temp();
        // Saves arriving in any order keep the lowest cap.
        store.set_host_cap("example.test", 1).unwrap();
        store.set_host_cap("example.test", 3).unwrap();
        assert_eq!(store.host_cap("example.test").unwrap(), Some(1));
        // Concurrent saves from clones (jobs) do too.
        let threads: Vec<_> = (1..=8)
            .rev()
            .map(|cap| {
                let store = store.clone();
                std::thread::spawn(move || store.set_host_cap("busy.test", cap).unwrap())
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        assert_eq!(store.host_cap("busy.test").unwrap(), Some(1));
    }

    #[test]
    fn stale_host_cap_is_ignored() {
        let (_dir, store) = open_temp();
        let day = 24 * 60 * 60;
        store
            .set_host_cap_at("old.test", 1, now_secs() - 8 * day)
            .unwrap();
        assert_eq!(store.host_cap("old.test").unwrap(), None);
        store
            .set_host_cap_at("recent.test", 2, now_secs() - 6 * day)
            .unwrap();
        assert_eq!(store.host_cap("recent.test").unwrap(), Some(2));
        // A new refusal starts it over, at the new cap.
        store.set_host_cap("old.test", 4).unwrap();
        assert_eq!(store.host_cap("old.test").unwrap(), Some(4));
    }
}
