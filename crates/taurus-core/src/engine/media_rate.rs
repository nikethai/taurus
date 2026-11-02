//! A media job's share of the speed limit. yt-dlp takes a fixed
//! `--limit-rate` when it starts, so the share is worked out for each run;
//! a change of the speed limit that moves the share of a running job by
//! more than 20 % restarts that job's yt-dlp (gracefully, as a pause does,
//! but the job stays Downloading) with the new rate. Other jobs starting or
//! stopping never do: only the limit setting does. Live recordings are never
//! restarted, since a stream cannot be taken up again where it was left.

use std::future::pending;

use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::errors::CoreError;
use crate::media::run::{Ended, MediaPhase, Output, run_process};
use crate::torrent::policy::split_rate;

use super::actor::Actor;
use super::read;
use super::settings::speed_limit;

/// What the limit change must move a share by to restart a run: more than
/// this many percent of the share it started with.
const RESTART_PERCENT: u64 = 20;

/// A running media job's rate, as the actor keeps it.
pub(super) struct MediaRate {
    /// The share (bytes per second; `None` without a limit) the job's
    /// current run has, or is about to start with.
    share: Option<u64>,
    /// The runner reads its next run's rate from here, and a run is
    /// restarted when it changes (`run_restartable`).
    tx: watch::Sender<Option<u64>>,
}

impl MediaRate {
    pub(super) fn new(share: Option<u64>) -> (MediaRate, watch::Receiver<Option<u64>>) {
        let (tx, rx) = watch::channel(share);
        (MediaRate { share, tx }, rx)
    }
}

/// Whether going from share `old` to `new` is worth a restart: more than 20 %
/// either way, or between limited and unlimited.
fn big_change(old: Option<u64>, new: Option<u64>) -> bool {
    match (old, new) {
        (None, None) => false,
        (Some(old), Some(new)) => {
            u128::from(old.abs_diff(new)) * 100 > u128::from(old) * u128::from(RESTART_PERCENT)
        }
        _ => true,
    }
}

impl Actor {
    /// The share of the speed limit for a media job about to start: it gets
    /// its part of the web downloads' rate as the jobs running now (and it)
    /// split it.
    pub(super) fn media_rate(&self) -> Option<u64> {
        let (http, torrents) = self.active();
        self.media_share(http + 1, torrents)
    }

    /// The part of the web downloads' rate for each of `jobs` sharing it,
    /// while `torrents` download too.
    fn media_share(&self, jobs: usize, torrents: usize) -> Option<u64> {
        let global = speed_limit(&self.settings, self.measured_peak);
        let (http_rate, _) = split_rate(global, true, torrents > 0);
        http_rate.map(|rate| rate / jobs.max(1) as u64)
    }

    /// Run after the effective speed limit changed (see `apply_limits`):
    /// every running media job whose share moved a lot restarts with the new
    /// rate. One in the middle of merging or converting is left alone: its
    /// limit no longer matters, and stopping it would waste the work.
    pub(super) fn retune_media(&mut self) {
        let (http, torrents) = self.active();
        let new = self.media_share(http, torrents);
        let ids: Vec<_> = self.media_rates.keys().copied().collect();
        for id in ids {
            let phase = read(&self.views)
                .get(&id)
                .and_then(|v| v.media.as_ref())
                .map(|m| m.phase);
            let waiting_for_tools = phase == Some(MediaPhase::GettingTools);
            let downloading = phase == Some(MediaPhase::Downloading);
            if !self.running.contains_key(&id) || !(downloading || waiting_for_tools) {
                continue;
            }
            if let Some(rate) = self.media_rates.get_mut(&id)
                && big_change(rate.share, new)
            {
                rate.share = new;
                rate.tx.send_replace(new);
            }
        }
    }
}

/// `run_process` that stops the run, as a pause does, when `rate` changes
/// from `started`, and says so (the second of the pair): the caller then
/// runs yt-dlp again with the new rate. A pause, removal or shutdown
/// (`cancel`) or `finish` is not a restart, whatever else was going on.
pub(super) async fn run_restartable(
    command: tokio::process::Command,
    cancel: &CancellationToken,
    finish: &CancellationToken,
    grace: std::time::Duration,
    mut rate: watch::Receiver<Option<u64>>,
    started: Option<u64>,
    on_output: impl FnMut(Output<'_>),
) -> Result<(Ended, bool), CoreError> {
    // Cancelled with `cancel`, and on its own for a restart.
    let pause = cancel.child_token();
    let changed = async {
        loop {
            // A live recording's sender is gone: it is never restarted.
            if rate.changed().await.is_err() {
                pending::<()>().await;
            }
            if *rate.borrow_and_update() != started {
                return;
            }
        }
    };
    let run = run_process(command, &pause, finish, grace, on_output);
    tokio::pin!(run, changed);
    let mut restarting = false;
    let ended = loop {
        tokio::select! {
            ended = &mut run => break ended?,
            () = &mut changed, if !restarting => {
                restarting = true;
                pause.cancel();
            }
        }
    };
    let restart = restarting && !cancel.is_cancelled() && !finish.is_cancelled();
    Ok((ended, restart))
}

#[cfg(test)]
mod tests {
    use super::big_change;

    #[test]
    fn restarts_only_on_big_changes() {
        // More than 20 % of the starting share, either way.
        assert!(big_change(Some(100), Some(79)));
        assert!(big_change(Some(100), Some(121)));
        assert!(!big_change(Some(100), Some(80)));
        assert!(!big_change(Some(100), Some(120)));
        assert!(!big_change(Some(100), Some(100)));
        // Limited and unlimited.
        assert!(big_change(Some(100), None));
        assert!(big_change(None, Some(100)));
        assert!(!big_change(None, None));
        // No overflow, no division by zero.
        assert!(big_change(Some(0), Some(1)));
        assert!(!big_change(Some(0), Some(0)));
        assert!(big_change(Some(u64::MAX), Some(1)));
    }
}
