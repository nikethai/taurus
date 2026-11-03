//! `Engine::job_detail`: what the Advanced screens show of one job beyond
//! its view. Cheap enough to poll twice a second: a web download's segments
//! come from its run while it runs (its live table) and from the store
//! otherwise; a torrent's files from its parsed metadata (parsed once per
//! torrent) and the session's per-file progress; a media job's files from
//! its view. Never a password: only whether one is kept.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};

use crate::http::LiveTable;
use crate::store::{JobId, JobKind, JobRecord, JobState, Store};
use crate::torrent::TorrentInfo;
use crate::torrent::files::is_selected;
use crate::torrent::hub::{TorrentHub, info_from_bytes};
use crate::torrent::policy::TorrentOverrides;

use super::keychain::Secrets;
use super::{Engine, JobView};

/// One job's details; see `Engine::job_detail`.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct JobDetail {
    pub id: JobId,
    /// A web download's byte ranges, each with what is written of it;
    /// empty for other jobs (and before the size is known).
    pub segments: Vec<SegmentView>,
    /// Connections (workers) of a web download running now; 0 otherwise.
    pub connections: u32,
    /// A torrent's files, by index; empty for other jobs (and for a magnet
    /// link whose metadata has not arrived).
    pub files: Vec<TorrentFileProgress>,
    /// A media job's finished files so far.
    pub media_files: Vec<PathBuf>,
    pub mirrors: Vec<String>,
    pub referer: Option<String>,
    pub checksum: Option<String>,
    /// Whether the job has a user name and a password is kept for it.
    /// The password itself is never part of the details.
    pub has_password: bool,
    /// A torrent's own sharing rules; `None` for other jobs.
    pub overrides: Option<TorrentOverrides>,
}

/// The byte range `[start, end)` of a web download, of which `done` bytes
/// from `start` are written.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub struct SegmentView {
    pub start: u64,
    pub end: u64,
    pub done: u64,
}

/// One file of a torrent: `done` of its `size` bytes are downloaded.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct TorrentFileProgress {
    pub index: usize,
    /// Path inside the torrent, `/`-separated.
    pub path: String,
    pub size: u64,
    pub done: u64,
    pub selected: bool,
}

/// A torrent's info-hash and its parsed metadata.
type ParsedTorrent = (String, Arc<TorrentInfo>);

/// What `job_detail` reads beside the views. Cheap to clone; clones share
/// everything.
#[derive(Clone)]
pub(super) struct Details {
    store: Store,
    /// The live table of each web download running now.
    live: Arc<Mutex<HashMap<JobId, LiveTable>>>,
    /// Each torrent's parsed metadata, by job and info-hash.
    torrents: Arc<Mutex<HashMap<JobId, ParsedTorrent>>>,
}

impl Details {
    pub(super) fn new(store: Store) -> Details {
        Details {
            store,
            live: Arc::default(),
            torrents: Arc::default(),
        }
    }

    /// A fresh live table for a run of the job that starts now.
    pub(super) fn track(&self, id: JobId) -> LiveTable {
        let table = LiveTable::default();
        lock(&self.live).insert(id, table.clone());
        table
    }

    /// Forgets the live table of a run that ended (unless a newer run of
    /// the job replaced it).
    pub(super) fn untrack(&self, id: JobId, table: &LiveTable) {
        let mut live = lock(&self.live);
        if live.get(&id).is_some_and(|t| t.same(table)) {
            live.remove(&id);
        }
    }

    fn detail(&self, view: &JobView, hub: &TorrentHub, secrets: &Secrets) -> Option<JobDetail> {
        let job = self.store.get_job(view.id).ok()??;
        let (segments, connections) = match job.kind {
            JobKind::Http => self.segments(job.id),
            _ => (Vec::new(), 0),
        };
        let files = match job.kind {
            JobKind::Torrent => self.torrent_files(&job, hub),
            _ => Vec::new(),
        };
        Some(JobDetail {
            id: job.id,
            segments,
            connections,
            files,
            media_files: view
                .media
                .as_ref()
                .map(|m| m.files.clone())
                .unwrap_or_default(),
            has_password: job.auth_user.is_some() && secrets.is_kept(job.id),
            overrides: (job.kind == JobKind::Torrent).then(|| job.torrent_overrides.clone()),
            mirrors: job.mirrors,
            referer: job.referer,
            checksum: job.checksum,
        })
    }

    /// The run's live table while it runs (once it has one), else the
    /// stored segments.
    fn segments(&self, id: JobId) -> (Vec<SegmentView>, u32) {
        let running = lock(&self.live).get(&id).map(LiveTable::get);
        let (segments, workers) = match running {
            Some(live) if !live.segments.is_empty() => (live.segments, live.workers),
            _ => (self.store.load_segments(id).unwrap_or_default(), 0),
        };
        let mut views: Vec<SegmentView> = segments
            .iter()
            .map(|s| SegmentView {
                start: s.start,
                end: s.end,
                done: s.done,
            })
            .collect();
        views.sort_by_key(|s| s.start);
        (views, workers)
    }

    /// The torrent's files with what the session has of each; without the
    /// session's count (not running, not checked yet), a torrent that
    /// finished downloading has all of each selected file, any other
    /// nothing known.
    fn torrent_files(&self, job: &JobRecord, hub: &TorrentHub) -> Vec<TorrentFileProgress> {
        let Some(info) = self.torrent_info(job) else {
            return Vec::new();
        };
        let only = job.only_files.as_deref();
        let progress = hub
            .file_progress(&info.info_hash)
            .filter(|p| p.len() == info.files.len());
        let finished = job.state == JobState::Completed || job.seed_started_at.is_some();
        info.files
            .iter()
            .map(|f| {
                let selected = is_selected(only, f.index);
                let done = match &progress {
                    Some(progress) => progress[f.index].min(f.size),
                    None if finished && selected => f.size,
                    None => 0,
                };
                TorrentFileProgress {
                    index: f.index,
                    path: f.path.clone(),
                    size: f.size,
                    done,
                    selected,
                }
            })
            .collect()
    }

    fn torrent_info(&self, job: &JobRecord) -> Option<Arc<TorrentInfo>> {
        let hash = job.info_hash.as_deref()?;
        if let Some((known, info)) = lock(&self.torrents).get(&job.id)
            && known == hash
        {
            return Some(info.clone());
        }
        let info = Arc::new(info_from_bytes(job.torrent_meta.as_deref()?).ok()?);
        lock(&self.torrents).insert(job.id, (hash.to_owned(), info.clone()));
        Some(info)
    }
}

impl Engine {
    /// The job's details for the Advanced screens (see [`JobDetail`]);
    /// `None` for a job the engine does not have. Cheap: meant to be
    /// polled (every 500 ms) while the details are shown. A running web
    /// download's segments are at most a quarter second old.
    pub fn job_detail(&self, id: JobId) -> Option<JobDetail> {
        let view = self.view(id)?;
        self.details.detail(&view, &self.hub, &self.secrets)
    }

    /// Deletes every password Taurus saved for this data dir's jobs (in
    /// `EngineConfig::secrets`: the OS keychain by default), including
    /// those of earlier versions that kept no list of them, and those held
    /// in memory; answers how many were removed. Runs after every password
    /// write asked for before it. Jobs keep their user names; one that
    /// signs in asks for its password again (`NeedsSignIn`). Fails if the
    /// engine has stopped, or with `Timeout` if the keychain does not
    /// answer within 30 seconds (the deletes go on).
    pub async fn forget_saved_passwords(&self) -> Result<u32, crate::errors::CoreError> {
        self.check_running()?;
        self.secrets.forget_all().await
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // Nothing panics while holding the lock, so the map is still whole.
    mutex.lock().unwrap_or_else(|p| p.into_inner())
}
