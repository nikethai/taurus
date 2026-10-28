//! The when-done action, run once the queue has nothing left to run: front
//! ends carry out Notify and Quit at once; Sleep and Shutdown run their OS
//! command after a countdown that new work or the user can cancel.

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use tokio::sync::broadcast;
use tokio::task::AbortHandle;
use tokio::time::Instant;

use crate::engine::Event;
use crate::settings::{PowerMode, WhenDone};
use crate::system::command_for;

/// Runs the when-done action; owned by the engine's actor.
pub(crate) struct WhenDoneRunner {
    events: broadcast::Sender<Event>,
    power: PowerMode,
    countdown: Duration,
    running: Option<Countdown>,
}

struct Countdown {
    /// Whether it may still fire or be cancelled. Held while announcing, so
    /// no tick follows `WhenDoneCancelled` and only one of firing and
    /// cancelling happens.
    live: Arc<Mutex<bool>>,
    task: AbortHandle,
}

impl WhenDoneRunner {
    pub(crate) fn new(
        events: broadcast::Sender<Event>,
        power: PowerMode,
        countdown: Duration,
    ) -> WhenDoneRunner {
        WhenDoneRunner {
            events,
            power,
            countdown,
            running: None,
        }
    }

    /// Runs `action` for a queue that has just gone idle.
    pub(crate) fn queue_idle(&mut self, action: WhenDone) {
        self.cancel();
        match action {
            WhenDone::Nothing => {}
            WhenDone::Notify | WhenDone::Quit => {
                let _ = self.events.send(Event::WhenDoneFired {
                    action,
                    command: None,
                });
            }
            WhenDone::Sleep | WhenDone::Shutdown => self.start(action),
        }
    }

    /// Stops a running countdown and announces `WhenDoneCancelled`; does
    /// nothing if none runs.
    pub(crate) fn cancel(&mut self) {
        let Some(countdown) = self.running.take() else {
            return;
        };
        countdown.task.abort();
        let mut live = lock(&countdown.live);
        if std::mem::replace(&mut *live, false) {
            let _ = self.events.send(Event::WhenDoneCancelled);
        }
    }

    /// Announces the countdown now and then once a second; fires when it
    /// runs out.
    fn start(&mut self, action: WhenDone) {
        let deadline = Instant::now() + self.countdown;
        let seconds = seconds_left(self.countdown);
        let _ = self.events.send(Event::WhenDoneCountdown {
            action,
            seconds_left: seconds,
        });
        let live = Arc::new(Mutex::new(true));
        let task = tokio::spawn(count_down(
            action,
            deadline,
            seconds,
            live.clone(),
            self.events.clone(),
            self.power,
        ))
        .abort_handle();
        self.running = Some(Countdown { live, task });
    }
}

impl Drop for WhenDoneRunner {
    fn drop(&mut self) {
        // Nothing fires once the engine has stopped.
        if let Some(countdown) = self.running.take() {
            *lock(&countdown.live) = false;
            countdown.task.abort();
        }
    }
}

/// Ticks at each whole second left after the first `seconds`, then runs
/// `action`'s command (only in `PowerMode::Execute`) and announces it.
async fn count_down(
    action: WhenDone,
    deadline: Instant,
    seconds: u64,
    live: Arc<Mutex<bool>>,
    events: broadcast::Sender<Event>,
    power: PowerMode,
) {
    for left in (1..seconds).rev() {
        tokio::time::sleep_until(deadline - Duration::from_secs(left)).await;
        let still_live = lock(&live);
        if !*still_live {
            return;
        }
        let _ = events.send(Event::WhenDoneCountdown {
            action,
            seconds_left: left,
        });
    }
    tokio::time::sleep_until(deadline).await;
    let command = command_for(action);
    {
        let mut still_live = lock(&live);
        if !std::mem::replace(&mut *still_live, false) {
            return;
        }
        let _ = events.send(Event::WhenDoneFired {
            action,
            command: command.as_ref().map(command_line),
        });
    }
    if power == PowerMode::Execute
        && let Some((program, args)) = command
    {
        run(program, args);
    }
}

/// Starts the command without waiting for it; a failure is logged.
fn run(program: String, args: Vec<String>) {
    match std::process::Command::new(&program).args(&args).spawn() {
        // Reaps it, to log a failure.
        Ok(mut child) => {
            std::thread::spawn(move || match child.wait() {
                Ok(status) if !status.success() => {
                    tracing::warn!("{program} failed: {status}");
                }
                Err(e) => tracing::warn!("could not wait for {program}: {e}"),
                Ok(_) => {}
            });
        }
        Err(e) => tracing::warn!("could not run {program}: {e}"),
    }
}

/// The program and its arguments, joined with spaces.
fn command_line((program, args): &(String, Vec<String>)) -> String {
    std::iter::once(program)
        .chain(args)
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(" ")
}

/// Whole seconds left, rounded up.
fn seconds_left(left: Duration) -> u64 {
    left.as_secs() + u64::from(left.subsec_nanos() > 0)
}

fn lock(live: &Mutex<bool>) -> MutexGuard<'_, bool> {
    // Nothing panics while holding the lock, so a poisoned value is still whole.
    live.lock().unwrap_or_else(|p| p.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seconds_left_rounds_up() {
        let left = |ms| seconds_left(Duration::from_millis(ms));
        assert_eq!(
            [left(0), left(1), left(300), left(1000), left(1001)],
            [0, 1, 1, 1, 2]
        );
        assert_eq!(left(60_000), 60);
    }

    #[test]
    fn command_line_joins_with_spaces() {
        let command = ("systemctl".to_string(), vec!["suspend".to_string()]);
        assert_eq!(command_line(&command), "systemctl suspend");
        assert_eq!(command_line(&("x".to_string(), Vec::new())), "x");
    }
}
