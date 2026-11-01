//! Retry and countdown delays, injectable so tests do not wait in real time.

use std::time::Duration;

/// Every delay and retry budget the engine waits on.
#[derive(Clone, Debug)]
pub struct Timing {
    /// First segment-retry delay; doubles per attempt.
    pub backoff_base: Duration,
    pub backoff_cap: Duration,
    /// Attempts for one segment before the job fails.
    pub max_segment_attempts: u32,
    /// First retry of a job waiting for the network; doubles up to the cap.
    pub waiting_retry_first: Duration,
    pub waiting_retry_cap: Duration,
    /// Notice period before a when-done action runs.
    pub when_done_countdown: Duration,
}

impl Default for Timing {
    fn default() -> Timing {
        Timing {
            backoff_base: Duration::from_millis(500),
            backoff_cap: Duration::from_secs(30),
            max_segment_attempts: 8,
            waiting_retry_first: Duration::from_secs(60),
            waiting_retry_cap: Duration::from_secs(600),
            when_done_countdown: Duration::from_secs(60),
        }
    }
}

impl Timing {
    /// Short delays so tests do not wait in real time.
    pub fn fast_for_tests() -> Timing {
        Timing {
            backoff_base: Duration::from_millis(10),
            backoff_cap: Duration::from_millis(50),
            max_segment_attempts: 2,
            waiting_retry_first: Duration::from_millis(100),
            waiting_retry_cap: Duration::from_millis(400),
            when_done_countdown: Duration::from_millis(300),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_test_values() {
        let t = Timing::default();
        assert_eq!(t.backoff_base, Duration::from_millis(500));
        assert_eq!(t.backoff_cap, Duration::from_secs(30));
        assert_eq!(t.max_segment_attempts, 8);
        assert_eq!(t.waiting_retry_first, Duration::from_secs(60));
        assert_eq!(t.waiting_retry_cap, Duration::from_secs(600));
        assert_eq!(t.when_done_countdown, Duration::from_secs(60));

        let f = Timing::fast_for_tests();
        assert_eq!(f.backoff_base, Duration::from_millis(10));
        assert_eq!(f.backoff_cap, Duration::from_millis(50));
        assert_eq!(f.max_segment_attempts, 2);
        assert_eq!(f.waiting_retry_first, Duration::from_millis(100));
        assert_eq!(f.waiting_retry_cap, Duration::from_millis(400));
        assert_eq!(f.when_done_countdown, Duration::from_millis(300));
    }
}
