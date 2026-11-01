//! Torrent jobs in the engine: reading what a torrent input names (a magnet
//! link, a `.torrent` file or a `.torrent` URL), adding the job, running it
//! in the queue and following it into Seeding, and removing its files.

use std::path::Path;
use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::errors::{CoreError, ErrorKind};
use crate::filename::{sanitize, unique_path};
use crate::http::JobOutcome;
use crate::http::client::{request_error, status_error};
use crate::store::{JobId, JobKind, JobRecord, JobState, NewJob};
use crate::torrent::files::{
    check_paths, check_selection, file_path, output_dir, selected_total, stored_name,
    stored_output_dir,
};
use crate::torrent::hub::info_from_bytes;
use crate::torrent::job::{TorrentJobContext, run_torrent_job};
use crate::torrent::source::{Source, classify};
use crate::torrent::{MAX_TORRENT_BYTES, TorrentInfo, read_torrent_file, too_large};

use super::actor::{Actor, Command, JobMsg, Run};
use super::jobs::{remove_if_exists, view_of};
use super::schedule::reserved_bytes;
use super::{AddRequest, Engine, Event, JobView, TorrentView, read, write};

/// A torrent to add, as read in the caller's task.
pub(super) struct TorrentInput {
    /// 40 lowercase hex characters.
    pub(super) info_hash: String,
    /// The torrent's name; for a magnet link its `dn`, else the info hash.
    pub(super) name: String,
    /// The metainfo bytes and what they say; `None` for a magnet link.
    pub(super) meta: Option<(Vec<u8>, TorrentInfo)>,
}

impl Engine {
    /// The files of the torrent `input` names (a magnet link, a `.torrent`
    /// URL or a local `.torrent` file), without adding it: for a file
    /// picker. A magnet link's metadata comes from its peers, which starts
    /// the torrent session; a magnet nobody serves within the metadata
    /// timeout is `NoPeers`. An unsafe torrent is `UnsafeTorrent`.
    pub async fn torrent_files(&self, input: &str) -> Result<TorrentInfo, CoreError> {
        self.check_running()?;
        let source = classify(input)?;
        if let Source::Magnet { .. } = source {
            let (info, _) = self.hub.list_files(&source, self.metadata_timeout).await?;
            check_paths(&info.files)?;
            return Ok(info);
        }
        let (_, info) = metainfo(&self.client, &source, None).await?;
        Ok(info)
    }

    /// Where the torrent session accepts peers; `None` until it starts.
    pub fn torrent_listen_addr(&self) -> Option<std::net::SocketAddr> {
        self.hub.listen_addr()
    }

    /// `add` for a torrent input, once `req.dest_dir` is checked: reads its
    /// metadata (a `.torrent` file or URL), or its info hash (a magnet link),
    /// and queues it.
    pub(super) async fn add_torrent(
        &self,
        mut req: AddRequest,
        source: Source,
    ) -> Result<JobId, CoreError> {
        if !req.mirrors.is_empty() || req.checksum.is_some() {
            return Err(error("mirrors and checksum apply only to web downloads"));
        }
        if let Some(only) = &mut req.only_files {
            only.sort_unstable();
            only.dedup();
        }
        let input = match source {
            Source::Magnet {
                info_hash, name, ..
            } => TorrentInput {
                name: name.unwrap_or_else(|| info_hash.clone()),
                info_hash,
                meta: None,
            },
            source => {
                let (bytes, info) = metainfo(&self.client, &source, req.referer.as_deref()).await?;
                TorrentInput {
                    info_hash: info.info_hash.clone(),
                    name: info.name.clone(),
                    meta: Some((bytes, info)),
                }
            }
        };
        let info = input.meta.as_ref().map(|(_, info)| info);
        match info {
            Some(info) => check_selection(info, req.only_files.as_deref())?,
            None if req.only_files.as_ref().is_some_and(Vec::is_empty) => {
                return Err(error("select at least one file"));
            }
            None => {}
        }
        self.call(|reply| Command::AddTorrent { req, input, reply })
            .await?
    }
}

/// The metainfo of a `.torrent` file or URL, checked to be safe.
async fn metainfo(
    client: &reqwest::Client,
    source: &Source,
    referer: Option<&str>,
) -> Result<(Vec<u8>, TorrentInfo), CoreError> {
    let bytes = match source {
        Source::TorrentFile(path) => read_torrent_file(path).await?,
        Source::TorrentUrl(url) => fetch_torrent(client, url, referer).await?,
        Source::Magnet { .. } | Source::Http(_) => {
            return Err(error("not a .torrent file or URL"));
        }
    };
    let info = info_from_bytes(&bytes)?;
    check_paths(&info.files)?;
    Ok((bytes, info))
}

/// The `.torrent` file at `url`, at most `MAX_TORRENT_BYTES`.
async fn fetch_torrent(
    client: &reqwest::Client,
    url: &url::Url,
    referer: Option<&str>,
) -> Result<Vec<u8>, CoreError> {
    let mut request = client.get(url.clone());
    if let Some(referer) = referer {
        request = request.header(reqwest::header::REFERER, referer);
    }
    let mut response = request.send().await.map_err(request_error)?;
    if !response.status().is_success() {
        return Err(status_error(response.status()));
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(request_error)? {
        if body.len() + chunk.len() > MAX_TORRENT_BYTES {
            return Err(too_large());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

impl Actor {
    /// Queues the torrent, unless a job already has it. A torrent whose
    /// metadata is known is placed now (its folder chosen, its entry in
    /// `dest_dir` recorded as `file_name`); a magnet link reserves its name
    /// and is placed once its metadata comes.
    pub(super) fn add_torrent(
        &mut self,
        req: AddRequest,
        input: TorrentInput,
    ) -> Result<JobId, CoreError> {
        if let Some(other) = self.store.find_by_info_hash(&input.info_hash)? {
            return Err(CoreError {
                kind: ErrorKind::AlreadyAdded,
                detail: format!("this torrent is already added as job {}", other.0),
            });
        }
        let hub = self.hub.clone();
        let placing = hub.placement_lock();
        let reserved = self.store.reserved_names(&req.dest_dir)?;
        let (meta, file_name, total) = match input.meta {
            Some((bytes, info)) => {
                let output = output_dir(&req.dest_dir, &info, &reserved);
                let total = selected_total(&info, req.only_files.as_deref());
                let name = stored_name(&req.dest_dir, &output, &info);
                (Some(bytes), name, Some(total))
            }
            None => {
                let name = sanitize(&input.name);
                let path = unique_path(&req.dest_dir, &name, &reserved);
                let name = path
                    .file_name()
                    .map_or(name, |n| n.to_string_lossy().into_owned());
                (None, name, None)
            }
        };
        let id = self.store.insert_job(&NewJob {
            url: req.url.clone(),
            source_url: req.url,
            dest_dir: req.dest_dir,
            file_name,
            connections: 1,
            mirrors: Vec::new(),
            checksum: None,
            referer: req.referer,
            queue_pos: 0,
            kind: JobKind::Torrent,
            info_hash: Some(input.info_hash),
            torrent_meta: meta,
            only_files: req.only_files,
        })?;
        drop(placing);
        let mut job = self.store.get_job(id)?.ok_or_else(super::no_such_job)?;
        if total.is_some() {
            job.total = total;
            if let Err(e) = self.store.update_job(&job) {
                if let Err(cleanup) = self.store.delete_job(id) {
                    tracing::warn!(job = id.0, "could not remove a half-added job: {cleanup}");
                }
                return Err(e);
            }
        }
        let view = view_of(&job, 0);
        write(&self.views).insert(id, view.clone());
        self.emit(Event::JobAdded(view));
        self.after_change();
        Ok(id)
    }

    /// Spawns the torrent's runner; the job is Downloading from here on, and
    /// holds a torrent slot until it is Seeding.
    pub(super) fn start_torrent(&mut self, id: JobId) {
        if let Err(e) = self.store.set_state(id, JobState::Downloading) {
            tracing::warn!(job = id.0, "could not record the start: {e}");
        }
        let downloaded_at_start = read(&self.views).get(&id).map_or(0, |v| v.downloaded);
        let cancel = CancellationToken::new();
        let views = self.views.clone();
        let ctx = TorrentJobContext {
            id,
            hub: self.hub.clone(),
            store: self.store.clone(),
            timing: self.timing.clone(),
            settings: self.torrent_settings.subscribe(),
            space: self.space,
            reserved: Arc::new(move |dir: &Path| reserved_bytes(&views, dir, Some(id))),
            cancel: cancel.clone(),
            msgs: self.job_tx.clone(),
        };
        let tx = self.job_tx.clone();
        tokio::spawn(async move {
            // A task of its own, so a panic still ends in `Finished`; the
            // runner's messages all precede it.
            let outcome = tokio::spawn(run_torrent_job(ctx))
                .await
                .unwrap_or_else(|e| {
                    JobOutcome::Failed(CoreError {
                        kind: ErrorKind::Unknown,
                        detail: format!("torrent task crashed: {e}"),
                    })
                });
            let _ = tx.send(JobMsg::Finished { id, outcome });
        });
        self.running.insert(
            id,
            Run {
                cancel,
                downloaded_at_start,
            },
        );
        self.set_view_state(id, JobState::Downloading);
    }

    /// The torrent finished downloading and shares (the runner recorded
    /// that): it leaves the torrent slots, and counts as a job finishing for
    /// the when-done action.
    pub(super) fn on_seeding(&mut self, id: JobId) {
        if !self.running.contains_key(&id) || self.state_of(id).is_none() {
            return;
        }
        self.set_view_state(id, JobState::Seeding);
        self.job_ended = true;
        self.after_change();
    }

    /// Whether the job is a torrent, as its view says.
    pub(super) fn is_torrent(&self, id: JobId) -> bool {
        read(&self.views)
            .get(&id)
            .is_some_and(|v| v.kind == JobKind::Torrent)
    }

    /// A torrent's view reloaded from the store keeps its last reported
    /// progress, short of Completed.
    pub(super) fn keep_torrent_progress(&self, view: &mut JobView) {
        if view.kind == JobKind::Torrent
            && view.state != JobState::Completed
            && let Some(before) = read(&self.views).get(&view.id)
        {
            view.downloaded = before.downloaded;
        }
    }

    pub(super) fn on_torrent_stats(&mut self, id: JobId, stats: TorrentView) {
        if let Some(view) = write(&self.views).get_mut(&id) {
            view.torrent = Some(stats);
        }
    }

    /// Takes the stopped torrent job's torrent out of the session and, with
    /// `delete_files`, deletes its files (only those of this torrent, of the
    /// size it gives them) and then the folders they leave empty, up to the
    /// folder the job was given. Spares the files if another job is recorded
    /// under the same name.
    pub(super) async fn delete_torrent(
        &self,
        job: &JobRecord,
        delete_files: bool,
    ) -> Result<(), CoreError> {
        let Some(hash) = job.info_hash.as_deref() else {
            return Ok(());
        };
        let delete_files = delete_files
            && !self
                .store
                .name_used_by_other(job.id, &job.dest_dir, &job.file_name)?;
        let info = job
            .torrent_meta
            .as_deref()
            .and_then(|m| info_from_bytes(m).ok());
        let output = info
            .as_ref()
            .map(|info| stored_output_dir(&job.dest_dir, &job.file_name, info));
        // librqbit also removes the torrent's folder once empty: a single
        // file saved straight into the save folder is deleted below
        // instead, so the save folder itself never goes.
        let session_deletes = delete_files && output.as_ref().is_some_and(|o| *o != job.dest_dir);
        // A session that is not running has nothing to drop: it reconciles
        // when it starts, and the files are deleted below.
        if self.hub.handle(hash).is_some()
            && let Err(e) = self.hub.delete(hash, session_deletes).await
        {
            tracing::warn!(job = job.id.0, "could not remove the torrent: {e}");
        }
        match (info, output) {
            (Some(info), Some(output)) if delete_files => {
                delete_files_of(&job.dest_dir, &output, &info)
            }
            _ => Ok(()),
        }
    }
}

/// Deletes what is left of the torrent's files in `output` (librqbit may not
/// have had the torrent, or was not asked to delete): each at its exact path,
/// if it has the size the torrent gives it or is empty (librqbit creates
/// every file, even one not selected). Then the folders they leave empty
/// inside `output`, and `output` itself if it is not `dest` and is empty:
/// `dest` is never removed.
fn delete_files_of(dest: &Path, output: &Path, info: &TorrentInfo) -> Result<(), CoreError> {
    for file in &info.files {
        let path = file_path(output, file);
        let ours = std::fs::metadata(&path)
            .is_ok_and(|m| m.is_file() && (m.len() == file.size || m.len() == 0));
        if ours {
            remove_if_exists(&path)?;
        }
        let mut dir = path.parent();
        while let Some(parent) = dir.filter(|d| d.starts_with(output) && *d != output) {
            // Fails, and so keeps it, unless empty.
            let _ = std::fs::remove_dir(parent);
            dir = parent.parent();
        }
    }
    if output != dest {
        let _ = std::fs::remove_dir(output);
    }
    Ok(())
}

/// A torrent's view from the store shows it fully downloaded once it
/// finished (it shares, or did); else the durable `downloaded`, which its
/// runner saves as it goes.
pub(super) fn stored_downloaded(job: &JobRecord, downloaded: u64) -> u64 {
    let finished = matches!(job.state, JobState::Seeding | JobState::Completed)
        || job.seed_started_at.is_some();
    match (job.kind, job.total) {
        (JobKind::Torrent, Some(total)) if finished => total,
        _ => downloaded,
    }
}

/// What only a torrent's view has, as stored: no peers, no speed.
pub(super) fn stored_torrent_view(job: &JobRecord) -> Option<TorrentView> {
    if job.kind != JobKind::Torrent {
        return None;
    }
    let info = job
        .torrent_meta
        .as_deref()
        .and_then(|m| info_from_bytes(m).ok());
    let (files_selected, files_total) = info.map_or((0, 0), |info| {
        let selected = info
            .files
            .iter()
            .filter(|f| job.only_files.as_ref().is_none_or(|o| o.contains(&f.index)))
            .count();
        (selected, info.files.len())
    });
    let total = job.total.unwrap_or(0);
    Some(TorrentView {
        peers: 0,
        uploaded: job.uploaded,
        upload_bps: 0,
        ratio: if total > 0 {
            job.uploaded as f64 / total as f64
        } else {
            0.0
        },
        files_selected: u32::try_from(files_selected).unwrap_or(u32::MAX),
        files_total: u32::try_from(files_total).unwrap_or(u32::MAX),
    })
}

fn error(detail: &str) -> CoreError {
    CoreError {
        kind: ErrorKind::Unknown,
        detail: detail.to_string(),
    }
}
