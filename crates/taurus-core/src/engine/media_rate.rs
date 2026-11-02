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
use crate::store::JobId;

/// What the limit change must move a share by to restart a run: more than
/// this many percent of the share it started with.
const RESTART_PERCENT: u64 = 20;

/// A running media job's rate, as the actor keeps it.
pub(super) struct MediaRate {
    /// The share (bytes per second; `None` without a limit) the job's
    /// current run has, or is about to start with.
    share: Option<u64>,
    /// The share the speed limit calls for now. It differs from `share`
    /// while a change waits for the job to be downloading again (see
    /// `apply_wanted`).
    wanted: Option<u64>,
    /// The runner reads its next run's rate from here, and a run is
    /// restarted when it changes (`run_restartable`).
    tx: watch::Sender<Option<u64>>,
}

impl MediaRate {
    pub(super) fn new(share: Option<u64>) -> (MediaRate, watch::Receiver<Option<u64>>) {
        let (tx, rx) = watch::channel(share);
        (
            MediaRate {
                share,
                wanted: share,
                tx,
            },
            rx,
        )
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
    /// every running media job learns the share it now calls for, and one
    /// whose share moved a lot restarts with the new rate (see
    /// `apply_wanted`).
    pub(super) fn retune_media(&mut self) {
        let (http, torrents) = self.active();
        let new = self.media_share(http, torrents);
        let ids: Vec<_> = self.media_rates.keys().copied().collect();
        for id in ids {
            if !self.running.contains_key(&id) {
                continue;
            }
            let phase = read(&self.views)
                .get(&id)
                .and_then(|v| v.media.as_ref())
                .map(|m| m.phase);
            if let Some(rate) = self.media_rates.get_mut(&id) {
                rate.wanted = new;
            }
            if let Some(phase) = phase {
                self.apply_wanted(id, phase);
            }
        }
    }

    /// Hands the job's runner the share the limit calls for if it moved a
    /// lot from the one the job has, and the job is in `phase` Downloading
    /// (or still getting its tools: nothing runs yet to restart). One that
    /// is merging or converting keeps it for later: its limit no longer
    /// matters and stopping it would waste the work. In a playlist that
    /// phase lasts until the next entry's first progress line, so this is
    /// also called when a job's phase becomes Downloading again.
    pub(super) fn apply_wanted(&mut self, id: JobId, phase: MediaPhase) {
        if !matches!(phase, MediaPhase::Downloading | MediaPhase::GettingTools) {
            return;
        }
        if let Some(rate) = self.media_rates.get_mut(&id)
            && big_change(rate.share, rate.wanted)
        {
            rate.share = rate.wanted;
            rate.tx.send_replace(rate.wanted);
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
    let mut ended = loop {
        tokio::select! {
            ended = &mut run => break ended?,
            () = &mut changed, if !restarting => {
                restarting = true;
                pause.cancel();
            }
        }
    };
    let restart = restarting && !cancel.is_cancelled() && !finish.is_cancelled();
    // yt-dlp finished by itself just as it was told to stop: that is a
    // success, not a restart (it is not run again over a finished download).
    if restart && ended.success() {
        ended.stopped = None;
        return Ok((ended, false));
    }
    Ok((ended, restart))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::sync::watch;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::engine::actor::{Actor, Run};
    use crate::engine::media::{MediaUpdate, MediaView};
    use crate::engine::write;
    use crate::engine::{EngineConfig, assemble};
    use crate::settings::SpeedLimit;
    use crate::store::{JobKind, JobState, NewJob, Store};

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

    /// A playlist stays Merging or Converting until the next entry's first
    /// progress line: a limit change in between is kept and applied then.
    #[tokio::test]
    async fn a_change_while_merging_is_applied_when_downloading_again() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        std::fs::create_dir_all(&data).unwrap();
        let store = Store::open(&data.join("taurus.db")).unwrap();
        let id = store
            .insert_job(&NewJob {
                url: "http://127.0.0.1:9/list".into(),
                source_url: "http://127.0.0.1:9/list".into(),
                dest_dir: "/nowhere".into(),
                file_name: "list".into(),
                connections: 1,
                mirrors: Vec::new(),
                checksum: None,
                referer: None,
                queue_pos: 0,
                kind: JobKind::Media,
                info_hash: None,
                torrent_meta: None,
                only_files: None,
            })
            .unwrap();
        drop(store);
        let mut config = EngineConfig::new(data);
        config.autostart = false;
        let (_engine, mut actor, _commands) = assemble(config).await.unwrap();
        let custom = |bytes_per_sec| SpeedLimit::Custom { bytes_per_sec };
        actor.settings.speed = custom(400_000);
        actor.media_global = Some(400_000);
        let update = |actor: &mut Actor, phase| {
            let update = MediaUpdate {
                phase,
                entries_done: 1,
                entries_total: 2,
                files: Vec::new(),
                live_elapsed_secs: None,
                announce: true,
            };
            actor.on_media_update(id, update);
        };
        write(&actor.views).get_mut(&id).unwrap().media = Some(MediaView {
            phase: MediaPhase::Merging,
            audio_only: false,
            live: false,
            entries_done: 0,
            entries_total: 2,
            live_elapsed_secs: None,
            starts_at: None,
            thumbnail: None,
            files: Vec::new(),
        });
        actor.running.insert(
            id,
            Run {
                cancel: CancellationToken::new(),
                downloaded_at_start: 0,
            },
        );
        actor.set_view_state(id, JobState::Downloading);
        let (rate, mut rx) = MediaRate::new(Some(400_000));
        actor.media_rates.insert(id, rate);

        for phase in [MediaPhase::Merging, MediaPhase::Converting] {
            actor.settings.speed = custom(100_000);
            update(&mut actor, phase);
            actor.apply_limits();
            assert!(!rx.has_changed().unwrap(), "{phase:?}: restarted at once");
            update(&mut actor, phase);
            assert!(!rx.has_changed().unwrap());
            // The next entry downloads: the new share is handed over.
            update(&mut actor, MediaPhase::Downloading);
            assert!(rx.has_changed().unwrap(), "{phase:?}: never applied");
            assert_eq!(*rx.borrow_and_update(), Some(100_000));
            // Back to a limit near the one the run has: nothing to do.
            actor.settings.speed = custom(110_000);
            actor.apply_limits();
            update(&mut actor, MediaPhase::Downloading);
            assert!(!rx.has_changed().unwrap());
            // For the next round: a big change from 100 kB/s.
            actor.settings.speed = custom(400_000);
            actor.apply_limits();
            assert_eq!(*rx.borrow_and_update(), Some(400_000));
            let (rate, new_rx) = MediaRate::new(Some(400_000));
            actor.media_rates.insert(id, rate);
            rx = new_rx;
        }
    }

    /// yt-dlp that finishes by itself as it is told to stop has succeeded:
    /// it is not run again over a finished download.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_finished_run_is_not_restarted() {
        let (tx, rx) = watch::channel(Some(1));
        let mut command = tokio::process::Command::new("sh");
        // Ignores the interrupt; says when it does, then ends well.
        command.args(["-c", "trap '' INT; echo ready; sleep 1; exit 0"]);
        command.stdin(std::process::Stdio::null());
        let (cancel, finish) = (CancellationToken::new(), CancellationToken::new());
        let (ended, restart) = run_restartable(
            command,
            &cancel,
            &finish,
            Duration::from_secs(30),
            rx,
            Some(1),
            |out| {
                if out == Output::Line("ready") {
                    tx.send_replace(Some(2));
                }
            },
        )
        .await
        .unwrap();
        assert!(ended.success(), "{ended:?}");
        assert_eq!(ended.stopped, None);
        assert!(!restart);
    }
}
