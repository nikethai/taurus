//! Applying settings in the actor, and the speed limit's glue: the
//! limiter's rate and the measured peak speed it is a share of.

use std::time::{Instant, SystemTime, UNIX_EPOCH};

use crate::errors::{CoreError, ErrorKind};
use crate::settings::{Settings, SpeedLimit, WhenDone};

use super::actor::Actor;
use super::{Event, write};

/// A measured peak older than this is replaced by the next measurement, even
/// a lower one.
const PEAK_MAX_AGE_SECS: i64 = 7 * 24 * 60 * 60;

impl Actor {
    pub(super) fn set_settings(&mut self, settings: Settings) -> Result<(), CoreError> {
        // Queued jobs, and their waiters, would wait for ever.
        if settings.max_active == 0 {
            return Err(CoreError {
                kind: ErrorKind::Unknown,
                detail: "max_active must be at least 1".to_string(),
            });
        }
        self.store.save_settings(&settings)?;
        if settings.when_done != self.settings.when_done {
            self.when_done.cancel();
        }
        self.limiter
            .set_rate(speed_limit(&settings, self.measured_peak));
        *write(&self.shared_settings) = settings.clone();
        self.settings = settings.clone();
        self.emit(Event::SettingsChanged(settings));
        self.after_change();
        Ok(())
    }

    /// Sleep and Shutdown are one-shot: once one fires, `when_done` goes
    /// back to Nothing, unless the user has already changed it.
    pub(super) fn on_when_done_fired(&mut self, action: WhenDone) {
        if self.settings.when_done != action {
            return;
        }
        let settings = Settings {
            when_done: WhenDone::Nothing,
            ..self.settings.clone()
        };
        if let Err(e) = self.set_settings(settings) {
            tracing::warn!("could not turn off the when-done action: {e}");
        }
    }

    /// Counts newly downloaded bytes and, while unlimited, saves a new peak.
    /// The tracker starts from the saved peak when that is fresh, so every
    /// peak it reports beats the saved one or replaces a stale one.
    pub(super) fn measure(&mut self, new_bytes: u64) {
        self.transferred = self.transferred.saturating_add(new_bytes);
        if self.settings.speed != SpeedLimit::Fastest {
            return;
        }
        let Some(bps) = self.peak.record(Instant::now(), self.transferred) else {
            return;
        };
        let at = unix_now();
        match self.store.save_measured_peak(bps, at) {
            Ok(()) => self.measured_peak = Some((bps, at)),
            Err(e) => tracing::warn!("could not save the peak speed: {e}"),
        }
    }
}

/// The limiter rate for `settings`: presets are shares of the saved peak.
pub(super) fn speed_limit(settings: &Settings, measured_peak: Option<(u64, i64)>) -> Option<u64> {
    settings
        .speed
        .bytes_per_sec(measured_peak.map(|(bps, _)| bps))
}

/// The saved peak, unless it is older than 7 days.
pub(super) fn fresh_peak(measured_peak: Option<(u64, i64)>, now: i64) -> Option<u64> {
    measured_peak
        .filter(|&(_, at)| now.saturating_sub(at) <= PEAK_MAX_AGE_SECS)
        .map(|(bps, _)| bps)
}

pub(super) fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_peak_older_than_seven_days_is_not_fresh() {
        let now = 1_700_000_000;
        let week = 7 * 24 * 60 * 60;
        assert_eq!(fresh_peak(None, now), None);
        assert_eq!(fresh_peak(Some((5, now - 3600)), now), Some(5));
        assert_eq!(fresh_peak(Some((5, now - week)), now), Some(5));
        assert_eq!(fresh_peak(Some((5, now - week - 1)), now), None);
    }
}
