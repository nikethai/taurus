//! Download speed estimate and progress-event throttling.

use std::time::{Duration, Instant};

/// Minimum spacing between emitted progress values (at most 4 per second).
const EMIT_INTERVAL: Duration = Duration::from_millis(250);
/// Time constant of the speed average: a sample's weight fades as e^(-age/τ).
const SPEED_TAU_SECS: f64 = 2.0;

/// Smooths download speed (time-weighted EWMA) and rate-limits progress events.
#[derive(Debug, Default)]
pub struct ProgressMeter {
    last_sample: Option<(Instant, u64)>,
    speed: Option<f64>,
    last_emit: Option<Instant>,
}

impl ProgressMeter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds the bytes downloaded so far. Returns the smoothed speed in
    /// bytes/s on the first call, then at most once per 250 ms.
    pub fn record(&mut self, now: Instant, downloaded: u64) -> Option<u64> {
        match self.last_sample {
            Some((at, before)) => {
                let dt = now.saturating_duration_since(at).as_secs_f64();
                if dt > 0.0 {
                    // A drop in `downloaded` (a restarted stream) counts as no progress.
                    let rate = downloaded.saturating_sub(before) as f64 / dt;
                    let alpha = 1.0 - (-dt / SPEED_TAU_SECS).exp();
                    self.speed = Some(self.speed.map_or(rate, |s| s + alpha * (rate - s)));
                    self.last_sample = Some((now, downloaded));
                }
            }
            None => self.last_sample = Some((now, downloaded)),
        }
        if self
            .last_emit
            .is_some_and(|t| now.saturating_duration_since(t) < EMIT_INTERVAL)
        {
            return None;
        }
        self.last_emit = Some(now);
        Some(self.speed.unwrap_or(0.0).round() as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn progress_meter_throttles() {
        let start = Instant::now();
        let mut meter = ProgressMeter::new();
        let emitted = (0..100u64)
            .filter(|&i| {
                let now = start + Duration::from_millis(i * 10);
                meter.record(now, i * 10_000).is_some()
            })
            .count();
        assert!((1..=5).contains(&emitted), "emitted {emitted}");
    }

    #[test]
    fn first_record_emits() {
        let mut meter = ProgressMeter::new();
        assert_eq!(meter.record(Instant::now(), 0), Some(0));
    }

    #[test]
    fn speed_tracks_a_steady_rate() {
        let start = Instant::now();
        let mut meter = ProgressMeter::new();
        let mut last = None;
        for i in 0..=30u64 {
            let now = start + Duration::from_millis(i * 100);
            if let Some(speed) = meter.record(now, i * 100_000) {
                last = Some(speed);
            }
        }
        assert_eq!(last, Some(1_000_000));
    }
}
