//! Numbers as short text. Sentences live in the Slint files; numbers and
//! dates are formatted here.

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

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// (year, month 1-12, day 1-31) of a day count since 1970-01-01.
fn civil_from_days(z: i64) -> (i64, usize, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month as usize, day)
}

/// Short moment for the Finished list: "14:05" on the day of `now_unix`,
/// else "Oct 28". Uses UTC for now (no local time zone lookup yet).
pub fn when_text(at_unix: i64, now_unix: i64) -> String {
    let day = at_unix.div_euclid(86_400);
    if day == now_unix.div_euclid(86_400) {
        let s = at_unix.rem_euclid(86_400);
        format!("{:02}:{:02}", s / 3600, s % 3600 / 60)
    } else {
        let (_, month, d) = civil_from_days(day);
        format!("{} {d}", MONTHS[month - 1])
    }
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
    fn when_text_cases() {
        // 2026-11-01 14:05:00 UTC
        let now = 1_793_541_900;
        assert_eq!(when_text(now - 3600, now), "13:05");
        assert_eq!(when_text(now - 7 * 86_400, now), "Oct 25");
        // 2024-02-29 00:00:00 UTC
        assert_eq!(when_text(1_709_164_800, now), "Feb 29");
    }
}
