use std::time::Duration;

use tokio_retry::strategy::ExponentialBackoff;

use crate::tau_err::TauError;

// Taken from agourlay/dlm library
pub fn retry_strategy(max_attempts: usize) -> impl Iterator<Item = Duration> {
    // usually implemented as `interval * factor^retry`
    // but tokio-retry does `interval^retry * factor`
    ExponentialBackoff::from_millis(10) // base interval in `interval^retry`
        .max_delay(Duration::from_secs(10 * 60)) // max 10 minutes
        .factor(1)
        .take(max_attempts) // limit retries
}

pub fn retry_handler(e: &TauError, link: &str) -> bool {
    let should_retry = is_network_error(e);
    if should_retry {
        let msg = format!("Scheduling retry for {} after error {:?}", link, e);
        println!("{}", msg);
    }
    should_retry
}

fn is_network_error(e: &TauError) -> bool {
    matches!(
        e,
        TauError::ConnectionClosed | TauError::ResponseBodyError | TauError::DeadLineElapsedTimeout
    )
}
