//! Numbers as short text. Sentences and words live in the Slint files;
//! numbers are formatted here, and dates are handed over as numbers.

const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];

/// Decimal (1000-based) size: "0 B", "999 B", "1.0 KB", "3.8 GB", "61 MB".
/// One decimal under 10, none from 10 up.
pub fn bytes(n: u64) -> String {
    if n < 1000 {
        return format!("{n} B");
    }
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 999.5 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    if value < 9.95 {
        format!("{value:.1} {}", UNITS[unit])
    } else {
        format!("{value:.0} {}", UNITS[unit])
    }
}

/// A speed: `bytes` per second, "3.0 MB/s".
pub fn speed(bps: u64) -> String {
    format!("{}/s", bytes(bps))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Eta {
    Unknown,
    UnderMinute,
    Minutes(u32),
    Hours(u32),
}

/// Time left for `remaining` bytes at `speed_bps`.
pub fn eta(remaining: u64, speed_bps: u64) -> Eta {
    if speed_bps == 0 {
        return Eta::Unknown;
    }
    let secs = remaining.div_ceil(speed_bps);
    if secs < 60 {
        Eta::UnderMinute
    } else if secs < 90 * 60 {
        Eta::Minutes(secs.div_ceil(60).min(u32::MAX as u64) as u32)
    } else {
        Eta::Hours(((secs as f64 / 3600.0).round()).min(u32::MAX as f64) as u32)
    }
}

/// The `eta-kind` / `eta-value` pair of the Slint view models.
pub fn eta_fields(e: Eta) -> (&'static str, i32) {
    match e {
        Eta::Unknown => ("unknown", 0),
        Eta::UnderMinute => ("under-minute", 0),
        Eta::Minutes(m) => ("minutes", m.min(i32::MAX as u32) as i32),
        Eta::Hours(h) => ("hours", h.min(i32::MAX as u32) as i32),
    }
}

/// Share ratio: "0.42" below 1, "1.0" up to 10, "12" from 10 up.
pub fn ratio(r: f64) -> String {
    if !r.is_finite() || r <= 0.0 {
        return "0.00".to_string();
    }
    if r < 1.0 {
        let s = format!("{r:.2}");
        return if s == "1.00" { "1.0".to_string() } else { s };
    }
    if r < 9.95 {
        format!("{r:.1}")
    } else {
        format!("{r:.0}")
    }
}

/// When a download finished, in the local time of `tz`, as numbers the
/// Finished list words (in `row.slint`): the clock time on the day of
/// `now_unix`, else the month and day.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Moment {
    pub same_day: bool,
    pub hour: u32,
    pub minute: u32,
    /// 1-12.
    pub month: u32,
    pub day: u32,
}

/// `None` for a time chrono cannot represent.
pub fn moment<Tz: chrono::TimeZone>(at_unix: i64, now_unix: i64, tz: &Tz) -> Option<Moment> {
    use chrono::{Datelike, Timelike};
    let local = |t| chrono::DateTime::from_timestamp(t, 0).map(|d| d.with_timezone(tz));
    let at = local(at_unix)?;
    let now = local(now_unix)?;
    Some(Moment {
        same_day: at.date_naive() == now.date_naive(),
        hour: at.hour(),
        minute: at.minute(),
        month: at.month(),
        day: at.day(),
    })
}

/// A length of time as a clock reads: "0:07", "3:32", "1:02:03".
pub fn duration(secs: u64) -> String {
    let (h, m, s) = (secs / 3600, secs / 60 % 60, secs % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// When something starts, in the local time of `tz`: "14:05" on the day of
/// `now_unix`, else "2026-11-02 14:05". `None` for a time chrono cannot
/// represent.
pub fn starts_at<Tz: chrono::TimeZone>(at_unix: i64, now_unix: i64, tz: &Tz) -> Option<String>
where
    Tz::Offset: std::fmt::Display,
{
    let local = |t| chrono::DateTime::from_timestamp(t, 0).map(|d| d.with_timezone(tz));
    let at = local(at_unix)?;
    let now = local(now_unix)?;
    let pattern = if at.date_naive() == now.date_naive() {
        "%H:%M"
    } else {
        "%Y-%m-%d %H:%M"
    };
    Some(at.format(pattern).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_cases() {
        assert_eq!(bytes(0), "0 B");
        assert_eq!(bytes(999), "999 B");
        assert_eq!(bytes(1000), "1.0 KB");
        assert_eq!(bytes(3_800_000_000), "3.8 GB");
        assert_eq!(bytes(61_000_000), "61 MB");
        assert_eq!(bytes(9_949), "9.9 KB");
        assert_eq!(bytes(9_960), "10 KB");
        assert_eq!(bytes(999_999), "1.0 MB");
        assert_eq!(bytes(u64::MAX), "18446744 TB");
    }

    #[test]
    fn eta_buckets() {
        assert_eq!(eta(1000, 0), Eta::Unknown);
        assert_eq!(eta(30 * 100, 100), Eta::UnderMinute);
        assert_eq!(eta(0, 100), Eta::UnderMinute);
        assert_eq!(eta(121 * 100, 100), Eta::Minutes(3));
        assert_eq!(eta(60 * 100, 100), Eta::Minutes(1));
        assert_eq!(eta(3 * 3600 * 100, 100), Eta::Hours(3));
        assert_eq!(eta(89 * 60 * 100, 100), Eta::Minutes(89));
        assert_eq!(eta(90 * 60 * 100, 100), Eta::Hours(2));
    }

    #[test]
    fn ratio_cases() {
        assert_eq!(ratio(0.0), "0.00");
        assert_eq!(ratio(0.42), "0.42");
        assert_eq!(ratio(0.999), "1.0");
        assert_eq!(ratio(1.0), "1.0");
        assert_eq!(ratio(2.46), "2.5");
        assert_eq!(ratio(12.3), "12");
        assert_eq!(ratio(f64::NAN), "0.00");
    }

    #[test]
    fn moment_in_local_time() {
        use chrono::{FixedOffset, Utc};
        // 2026-11-01 14:05:00 UTC
        let now = 1_793_541_900;
        let m = moment(now - 3600, now, &Utc).unwrap();
        assert_eq!((m.same_day, m.hour, m.minute), (true, 13, 5));
        let m = moment(now - 7 * 86_400, now, &Utc).unwrap();
        assert_eq!((m.same_day, m.month, m.day), (false, 10, 25));
        // 2024-02-29 00:00:00 UTC
        let m = moment(1_709_164_800, now, &Utc).unwrap();
        assert_eq!((m.same_day, m.month, m.day), (false, 2, 29));

        // 23:30 UTC on 31 Oct and 00:30 UTC on 1 Nov are both 1 Nov at
        // UTC+11: the same day there, though not in UTC.
        let plus11 = FixedOffset::east_opt(11 * 3600).unwrap();
        let half_past = now - (13 * 3600 + 35 * 60); // 2026-11-01 00:30 UTC
        assert!(!moment(half_past - 3600, half_past, &Utc).unwrap().same_day);
        let m = moment(half_past - 3600, half_past, &plus11).unwrap();
        assert_eq!((m.same_day, m.hour, m.minute), (true, 10, 30));
        // Past midnight local, the same instant is another day.
        let minus5 = FixedOffset::west_opt(5 * 3600).unwrap();
        let m = moment(now - 10 * 3600, now, &minus5).unwrap();
        assert_eq!((m.same_day, m.hour, m.month, m.day), (false, 23, 10, 31));
        assert_eq!(moment(i64::MAX, now, &Utc), None);
    }

    #[test]
    fn durations() {
        assert_eq!(duration(0), "0:00");
        assert_eq!(duration(7), "0:07");
        assert_eq!(duration(212), "3:32");
        assert_eq!(duration(3723), "1:02:03");
        assert_eq!(duration(36_000), "10:00:00");
    }

    #[test]
    fn start_times_in_local_time() {
        use chrono::{FixedOffset, Utc};
        // 2026-11-01 14:05:00 UTC
        let now = 1_793_541_900;
        assert_eq!(starts_at(now + 3600, now, &Utc).as_deref(), Some("15:05"));
        assert_eq!(
            starts_at(now + 86_400, now, &Utc).as_deref(),
            Some("2026-11-02 14:05")
        );
        // 23:00 UTC is the next day at UTC+2.
        let plus2 = FixedOffset::east_opt(2 * 3600).unwrap();
        assert_eq!(
            starts_at(now + 8 * 3600 + 55 * 60, now, &plus2).as_deref(),
            Some("2026-11-02 01:00")
        );
        assert_eq!(starts_at(i64::MAX, now, &Utc), None);
    }
}
