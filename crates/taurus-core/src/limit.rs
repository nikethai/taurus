//! One speed limit shared by every download worker, and the measurement of
//! the line's peak speed.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use tokio::sync::Notify;

/// The bucket holds one second of the rate, but never less than this.
const MIN_CAPACITY: u64 = 64 * 1024;
/// A limit of 0 bytes/s (Custom 0, or a preset of a tiny measured peak) is
/// raised to this, so a limited download never stalls for good.
const MIN_RATE: u64 = 1024;
/// Guards against waking over and over on a rounding remainder.
const MIN_WAIT: Duration = Duration::from_millis(1);
/// The peak is the best average over this window...
const PEAK_WINDOW: Duration = Duration::from_secs(5);
/// ...once the samples span at least this long.
const PEAK_MIN_SPAN: Duration = Duration::from_secs(1);

/// A token bucket shared by every worker of every job. Clones share one
/// bucket, so the limit applies to all downloads together.
#[derive(Clone)]
pub struct Limiter {
    shared: Arc<Shared>,
}

struct Shared {
    /// Bytes per second; 0 is unlimited. Written under the bucket's lock.
    rate: AtomicU64,
    bucket: Mutex<Bucket>,
    /// Wakes waiting acquires when the rate changes.
    changed: Notify,
}

struct Bucket {
    tokens: f64,
    refilled: tokio::time::Instant,
}

impl Bucket {
    fn refill(&mut self, rate: u64, now: tokio::time::Instant) {
        if rate != 0 {
            let elapsed = now.saturating_duration_since(self.refilled).as_secs_f64();
            self.tokens = (self.tokens + elapsed * rate as f64).min(capacity(rate) as f64);
        }
        self.refilled = now;
    }
}

impl Limiter {
    /// `None` is unlimited; `Some(0)` is raised to 1 KiB/s. The bucket
    /// starts empty.
    pub fn new(bytes_per_sec: Option<u64>) -> Limiter {
        Limiter {
            shared: Arc::new(Shared {
                rate: AtomicU64::new(to_rate(bytes_per_sec)),
                bucket: Mutex::new(Bucket {
                    tokens: 0.0,
                    refilled: tokio::time::Instant::now(),
                }),
                changed: Notify::new(),
            }),
        }
    }

    /// Changes the limit, also for acquires already waiting. Saved tokens
    /// are kept up to the new capacity.
    pub fn set_rate(&self, bytes_per_sec: Option<u64>) {
        let rate = to_rate(bytes_per_sec);
        {
            let mut bucket = self.lock();
            let old = self.shared.rate.load(Ordering::Acquire);
            bucket.refill(old, tokio::time::Instant::now());
            if rate != 0 {
                bucket.tokens = bucket.tokens.min(capacity(rate) as f64);
            }
            self.shared.rate.store(rate, Ordering::Release);
        }
        self.shared.changed.notify_waiters();
    }

    /// Waits until the limit allows `bytes` more; returns at once when
    /// unlimited. More than the bucket holds is taken in bucket-sized pieces.
    /// Cancel-safe: dropping the future takes no tokens beyond the pieces
    /// already granted.
    pub async fn acquire(&self, bytes: u64) {
        let mut left = bytes;
        while left > 0 {
            if self.shared.rate.load(Ordering::Acquire) == 0 {
                return;
            }
            // Created before the rate is read again, so a change after that
            // read still wakes this wait.
            let changed = self.shared.changed.notified();
            let wait = {
                let mut bucket = self.lock();
                let rate = self.shared.rate.load(Ordering::Acquire);
                if rate == 0 {
                    return;
                }
                bucket.refill(rate, tokio::time::Instant::now());
                let piece = left.min(capacity(rate));
                if bucket.tokens >= piece as f64 {
                    bucket.tokens -= piece as f64;
                    left -= piece;
                    continue;
                }
                let missing = piece as f64 - bucket.tokens;
                Duration::from_secs_f64(missing / rate as f64).max(MIN_WAIT)
            };
            tokio::select! {
                () = tokio::time::sleep(wait) => {}
                () = changed => {}
            }
        }
    }

    fn lock(&self) -> MutexGuard<'_, Bucket> {
        // A panic cannot leave the two fields inconsistent.
        self.shared.bucket.lock().unwrap_or_else(|p| p.into_inner())
    }
}

fn to_rate(bytes_per_sec: Option<u64>) -> u64 {
    bytes_per_sec.map_or(0, |rate| rate.max(MIN_RATE))
}

fn capacity(rate: u64) -> u64 {
    rate.max(MIN_CAPACITY)
}

/// The best 5-second average of the bytes downloaded by all jobs together.
#[derive(Debug, Default)]
pub struct PeakTracker {
    /// `(when, total)`, oldest first, spanning at most the window.
    samples: VecDeque<(Instant, u64)>,
    max: u64,
}

impl PeakTracker {
    pub fn new() -> PeakTracker {
        PeakTracker::default()
    }

    /// A tracker that reports only averages above `max` (bytes/s).
    pub fn with_max(max: u64) -> PeakTracker {
        PeakTracker {
            max,
            ..PeakTracker::default()
        }
    }

    /// Feeds the running total of bytes downloaded. Returns the average over
    /// the last 5 s when it beats every earlier one, once the samples span at
    /// least 1 s. A total that goes back starts a new window.
    pub fn record(&mut self, now: Instant, total_downloaded_all_jobs: u64) -> Option<u64> {
        let total = total_downloaded_all_jobs;
        if self
            .samples
            .back()
            .is_some_and(|&(at, before)| total < before || now < at)
        {
            self.samples.clear();
        }
        self.samples.push_back((now, total));
        while self
            .samples
            .front()
            .is_some_and(|&(at, _)| now.duration_since(at) > PEAK_WINDOW)
        {
            self.samples.pop_front();
        }
        let &(first_at, first) = self.samples.front()?;
        let span = now.duration_since(first_at);
        if span < PEAK_MIN_SPAN {
            return None;
        }
        let average = ((total - first) as f64 / span.as_secs_f64()).round() as u64;
        (average > self.max).then(|| {
            self.max = average;
            average
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    const PIECE: u64 = 64 * 1024;

    #[tokio::test(start_paused = true)]
    async fn limiter_unlimited_does_not_wait() {
        let limiter = Limiter::new(None);
        let start = tokio::time::Instant::now();
        for _ in 0..100 {
            limiter.acquire(10 * 1024 * 1024).await;
        }
        assert_eq!(start.elapsed(), Duration::ZERO);
    }

    #[tokio::test(start_paused = true)]
    async fn limiter_enforces_rate() {
        let limiter = Limiter::new(Some(1_000_000));
        let start = tokio::time::Instant::now();
        let mut left: u64 = 3_000_000;
        while left > 0 {
            let piece = left.min(PIECE);
            limiter.acquire(piece).await;
            left -= piece;
        }
        let elapsed = start.elapsed();
        assert!(
            elapsed >= Duration::from_millis(1900) && elapsed <= Duration::from_millis(3100),
            "took {elapsed:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn limiter_is_shared_by_clones() {
        let limiter = Limiter::new(Some(1_000_000));
        let start = tokio::time::Instant::now();
        let workers: Vec<_> = (0..4)
            .map(|_| {
                let limiter = limiter.clone();
                tokio::spawn(async move {
                    for _ in 0..8 {
                        limiter.acquire(PIECE).await;
                    }
                })
            })
            .collect();
        for worker in workers {
            worker.await.unwrap();
        }
        // 32 pieces of 64 KiB at 1 MB/s together.
        let elapsed = start.elapsed();
        assert!(
            elapsed >= Duration::from_millis(2000) && elapsed <= Duration::from_millis(2200),
            "took {elapsed:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn set_rate_applies_live() {
        let limiter = Limiter::new(Some(100_000));
        let worker = {
            let limiter = limiter.clone();
            tokio::spawn(async move {
                let mut done_at = Vec::new();
                for _ in 0..40 {
                    limiter.acquire(PIECE).await;
                    done_at.push(tokio::time::Instant::now());
                }
                done_at
            })
        };
        let start = tokio::time::Instant::now();
        tokio::time::sleep(Duration::from_secs(2)).await;
        limiter.set_rate(None);
        let done_at = worker.await.unwrap();
        // Limited: at most 200 kB in the first 2 s, i.e. three pieces.
        let early = done_at
            .iter()
            .filter(|t| t.duration_since(start) < Duration::from_secs(2))
            .count();
        assert!((1..=3).contains(&early), "{early} pieces before the switch");
        // Unlimited: the rest, including the one waiting, at once.
        assert_eq!(
            done_at.last().unwrap().duration_since(start),
            Duration::from_secs(2)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn set_rate_keeps_tokens_up_to_the_new_capacity() {
        let limiter = Limiter::new(Some(1_000_000));
        // The bucket starts empty and fills to one second of the rate.
        tokio::time::sleep(Duration::from_secs(5)).await;
        limiter.set_rate(Some(100_000));
        let start = tokio::time::Instant::now();
        limiter.acquire(100_000).await;
        assert_eq!(start.elapsed(), Duration::ZERO);
        // The new capacity is one second of 100 kB/s, and that used it up.
        limiter.acquire(50_000).await;
        let elapsed = start.elapsed();
        assert!(
            elapsed >= Duration::from_millis(490) && elapsed <= Duration::from_millis(520),
            "took {elapsed:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn zero_rate_and_oversized_requests_still_finish() {
        // A zero rate is raised to 1 KiB/s; a request larger than the bucket
        // is served piece by piece.
        let limiter = Limiter::new(Some(0));
        let start = tokio::time::Instant::now();
        limiter.acquire(200 * 1024).await;
        let elapsed = start.elapsed();
        assert!(
            elapsed >= Duration::from_secs(199) && elapsed <= Duration::from_secs(201),
            "took {elapsed:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn cancelled_acquire_takes_no_tokens() {
        let limiter = Limiter::new(Some(100_000));
        let cancelled =
            tokio::time::timeout(Duration::from_millis(100), limiter.acquire(PIECE)).await;
        assert!(cancelled.is_err());
        // The bucket kept filling from 0 meanwhile: 50 kB after 0.5 s in all.
        let start = tokio::time::Instant::now();
        limiter.acquire(50_000).await;
        let elapsed = start.elapsed();
        assert!(
            elapsed >= Duration::from_millis(390) && elapsed <= Duration::from_millis(410),
            "took {elapsed:?}"
        );
    }

    #[test]
    fn peak_tracker_reports_new_max_only() {
        let start = Instant::now();
        let mut tracker = PeakTracker::new();
        let mut total = 0;
        let mut feed = |tracker: &mut PeakTracker, step: u64, bytes_per_step: u64| {
            total += bytes_per_step;
            tracker.record(start + Duration::from_millis(250 * step), total)
        };
        // 1 MB/s for 3 s: nothing before a full second of samples, then the
        // average once, then nothing while it does not grow.
        let first: Vec<Option<u64>> = (0..=12).map(|i| feed(&mut tracker, i, 250_000)).collect();
        assert_eq!(first[..4], [None; 4]);
        assert_eq!(first[4], Some(1_000_000));
        assert!(first[5..].iter().all(Option::is_none), "{first:?}");
        // Faster: new maxima while the 5 s average climbs.
        let faster: Vec<Option<u64>> = (13..=40).map(|i| feed(&mut tracker, i, 500_000)).collect();
        let reported: Vec<u64> = faster.iter().flatten().copied().collect();
        assert!(reported.windows(2).all(|w| w[0] < w[1]), "{reported:?}");
        assert_eq!(reported.last(), Some(&2_000_000));
        // Slower again: never reported.
        assert!((41..=80).all(|i| feed(&mut tracker, i, 100_000).is_none()));
    }

    #[test]
    fn peak_tracker_starts_from_a_given_max_and_survives_a_reset_counter() {
        let start = Instant::now();
        let mut tracker = PeakTracker::with_max(3_000_000);
        // 2 MB/s does not beat the stored 3 MB/s.
        assert!((0..=20).all(|i| {
            tracker
                .record(start + Duration::from_millis(250 * i), i * 500_000)
                .is_none()
        }));
        // A counter that goes back starts a new window: the first sample of
        // it reports nothing, and 4 MB/s afterwards beats the max.
        let reported: Vec<u64> = (21..=30)
            .filter_map(|i| {
                tracker.record(start + Duration::from_millis(250 * i), (i - 21) * 1_000_000)
            })
            .collect();
        assert_eq!(reported, [4_000_000]);
    }
}
