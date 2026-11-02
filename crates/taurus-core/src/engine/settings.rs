//! Applying settings in the actor, and the speed limit's glue: the
//! limiter's rate, the torrent session's limits and the measured peak speed
//! they are a share of.

use std::time::{Instant, SystemTime, UNIX_EPOCH};

use crate::errors::{CoreError, ErrorKind};
use crate::network::torrent_proxy;
use crate::settings::{Settings, SpeedLimit, WhenDone};
use crate::torrent::policy::{split_rate, torrent_rate};

use super::actor::Actor;
use super::{Event, write};

/// A measured peak older than this is replaced by the next measurement, even
/// a lower one.
const PEAK_MAX_AGE_SECS: i64 = 7 * 24 * 60 * 60;

impl Actor {
    pub(super) fn set_settings(&mut self, settings: Settings) -> Result<(), CoreError> {
        // Queued jobs, and their waiters, would wait for ever.
        for (value, name) in [
            (settings.max_active, "max_active"),
            (settings.torrent.max_active, "torrent.max_active"),
        ] {
            if value == 0 {
                return Err(CoreError {
                    kind: ErrorKind::Unknown,
                    detail: format!("{name} must be at least 1"),
                });
            }
        }
        // Refused now, not at the first download.
        settings.network.proxy.check()?;
        self.store.save_settings(&settings)?;
        if settings.when_done != self.settings.when_done {
            self.when_done.cancel();
        }
        // Every web request from now on, and the torrent session's next
        // start (a running one keeps its proxy).
        if settings.network.proxy != self.settings.network.proxy {
            self.client.set_proxy(&settings.network.proxy);
            self.hub.set_proxy(torrent_proxy(&settings.network.proxy));
        }
        *write(&self.shared_settings) = settings.clone();
        // Running torrents follow the new seeding policy at once.
        self.torrent_settings.send_replace(settings.torrent.clone());
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

    /// Splits the speed limit between HTTP jobs and torrents while both
    /// download (a torrent-only limit caps the torrent side further), and
    /// applies it, with the upload limit, if anything changed.
    pub(super) fn apply_limits(&mut self) {
        let (http, torrents) = self.active();
        let global = speed_limit(&self.settings, self.measured_peak);
        // Only a change of the limit itself reaches running video downloads,
        // not jobs coming and going.
        if global != self.media_global {
            self.media_global = global;
            self.retune_media();
        }
        let (http_rate, torrent_share) = split_rate(global, http > 0, torrents > 0);
        let torrent_rate = torrent_rate(torrent_share, self.settings.torrent.download_limit_bps);
        let rates = (
            http_rate,
            torrent_rate,
            self.settings.torrent.upload_limit_bps,
        );
        if self.rates == Some(rates) {
            return;
        }
        self.rates = Some(rates);
        self.limiter.set_rate(http_rate);
        self.hub.set_limits(torrent_rate, rates.2);
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
    use crate::engine::actor::Run;
    use crate::engine::{EngineConfig, assemble};
    use crate::store::{JobId, JobKind, JobState, NewJob, Store};
    use tokio_util::sync::CancellationToken;

    fn job(kind: JobKind, name: &str) -> NewJob {
        NewJob {
            url: format!("http://127.0.0.1:9/{name}"),
            source_url: format!("http://127.0.0.1:9/{name}"),
            dest_dir: "/nowhere".into(),
            file_name: name.into(),
            connections: 1,
            mirrors: Vec::new(),
            checksum: None,
            referer: None,
            queue_pos: 0,
            kind,
            info_hash: None,
            torrent_meta: None,
            only_files: None,
        }
    }

    fn run(actor: &mut Actor, id: JobId, state: JobState) {
        let run = Run {
            cancel: CancellationToken::new(),
            downloaded_at_start: 0,
        };
        actor.running.insert(id, run);
        actor.set_view_state(id, state);
    }

    #[tokio::test]
    async fn the_limit_is_split_while_both_kinds_download() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        std::fs::create_dir_all(&data).unwrap();
        let store = Store::open(&data.join("taurus.db")).unwrap();
        let http = store.insert_job(&job(JobKind::Http, "a.bin")).unwrap();
        let torrent = store.insert_job(&job(JobKind::Torrent, "t")).unwrap();
        drop(store);
        let mut config = EngineConfig::new(data);
        config.autostart = false;
        let (_engine, mut actor, _commands) = assemble(config).await.unwrap();
        actor.settings.speed = SpeedLimit::Custom {
            bytes_per_sec: 1_000_000,
        };
        actor.settings.torrent.download_limit_bps = Some(300_000);
        actor.settings.torrent.upload_limit_bps = Some(50_000);

        actor.apply_limits();
        assert_eq!(
            actor.rates,
            Some((Some(1_000_000), Some(300_000), Some(50_000)))
        );
        run(&mut actor, http, JobState::Downloading);
        run(&mut actor, torrent, JobState::Downloading);
        actor.apply_limits();
        assert_eq!(
            actor.rates,
            Some((Some(500_000), Some(300_000), Some(50_000)))
        );
        actor.settings.torrent.download_limit_bps = None;
        actor.apply_limits();
        assert_eq!(
            actor.rates,
            Some((Some(500_000), Some(500_000), Some(50_000)))
        );
        // A seeding torrent downloads nothing: HTTP gets it all.
        actor.set_view_state(torrent, JobState::Seeding);
        actor.apply_limits();
        assert_eq!(
            actor.rates,
            Some((Some(1_000_000), Some(1_000_000), Some(50_000)))
        );
    }

    #[tokio::test]
    async fn a_new_proxy_reaches_the_next_torrent_session() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = EngineConfig::new(dir.path().join("data"));
        config.autostart = false;
        config.secrets = std::sync::Arc::new(crate::secrets::MemoryStore::new());
        let (_engine, mut actor, _commands) = assemble(config).await.unwrap();
        assert_eq!(actor.hub.proxy(), None);
        let socks = crate::network::ProxySetting::Manual {
            url: "socks5://127.0.0.1:9050".to_string(),
        };
        let mut settings = actor.settings.clone();
        settings.network.proxy = socks;
        actor.set_settings(settings.clone()).unwrap();
        assert_eq!(
            actor.hub.proxy().as_deref(),
            Some("socks5://127.0.0.1:9050")
        );
        // An http proxy is not one for torrents.
        settings.network.proxy = crate::network::ProxySetting::Manual {
            url: "http://127.0.0.1:3128".to_string(),
        };
        actor.set_settings(settings).unwrap();
        assert_eq!(actor.hub.proxy(), None);
    }

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
