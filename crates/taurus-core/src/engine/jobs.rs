//! A job's lifecycle in the actor: adding, pausing, resuming (and
//! resolving an expired link again), removing, and handling a finished
//! run; with the checks they rely on (mirror vetting, change detection).

use std::path::{Path, PathBuf};

use tokio_util::sync::CancellationToken;

use crate::errors::{CoreError, ErrorKind};
use crate::filename::{PARTIAL_EXT, sanitize, unique_path};
use crate::fixups;
use crate::http::probe::validator_change;
use crate::http::{JobOutcome, Probe, resolve};
use crate::plan::DEFAULT_CONNECTIONS;
use crate::store::{JobId, JobRecord, JobState, NewJob, Store};
use crate::system::marks::loggable;
use crate::system::{SpaceSource, check_fits, same_volume};

use super::actor::{Actor, JobMsg, Run};
use super::{AddRequest, Event, JobView, Views, io_error, no_such_job, read, write};

impl Actor {
    pub(super) fn add(&mut self, req: AddRequest, probe: &Probe) -> Result<JobId, CoreError> {
        let name = match &req.file_name {
            Some(name) => sanitize(name),
            None => probe.file_name.clone(),
        };
        // Reserving here, in the single actor, keeps concurrent adds apart.
        let reserved = self.store.reserved_names(&req.dest_dir)?;
        let path = unique_path(&req.dest_dir, &name, &reserved);
        let file_name = path
            .file_name()
            .map_or(name, |n| n.to_string_lossy().into_owned());
        let connections = req.connections.unwrap_or(DEFAULT_CONNECTIONS).max(1);
        // The link that served the file (a confirm form's, if one was
        // followed), so segment requests skip the page.
        let url = probe.requested_url.to_string();
        let id = self.store.insert_job(&NewJob {
            url: url.clone(),
            source_url: req.url.clone(),
            dest_dir: req.dest_dir.clone(),
            file_name: file_name.clone(),
            connections,
            mirrors: req.mirrors.clone(),
            checksum: req.checksum.clone(),
            referer: req.referer.clone(),
            queue_pos: 0,
        })?;
        let mut job = JobRecord {
            id,
            url,
            source_url: req.url,
            final_url: None,
            dest_dir: req.dest_dir,
            file_name,
            state: JobState::Queued,
            total: None,
            resumable: false,
            etag: None,
            last_modified: None,
            connections,
            error: None,
            mirrors: req.mirrors,
            checksum: req.checksum,
            referer: req.referer,
            queue_pos: id.0,
        };
        apply_probe(&mut job, probe);
        if let Err(e) = self.store.update_job(&job) {
            if let Err(cleanup) = self.store.delete_job(id) {
                tracing::warn!(job = id.0, "could not remove a half-added job: {cleanup}");
            }
            return Err(e);
        }
        let view = view_of(&job, 0);
        write(&self.views).insert(id, view.clone());
        self.emit(Event::JobAdded(view));
        self.after_change();
        Ok(id)
    }

    pub(super) async fn pause(&mut self, id: JobId) -> Result<(), CoreError> {
        if !read(&self.views).contains_key(&id) {
            return Err(no_such_job());
        }
        if let Some(run) = self.running.get(&id) {
            run.cancel.cancel();
            self.settle(&[id]).await;
        }
        // Not running, or it failed (and now waits) as it stopped.
        let result = if matches!(
            self.state_of(id),
            Some(JobState::Queued | JobState::Waiting)
        ) {
            self.cancel_retry(id);
            self.store
                .set_state(id, JobState::Paused)
                .map(|()| self.set_view_state(id, JobState::Paused))
        } else {
            Ok(())
        };
        self.retry_counts.remove(&id);
        self.after_change();
        result
    }

    pub(super) fn resume(&mut self, id: JobId, probe: &Probe) -> Result<(), CoreError> {
        let job = self.store.get_job(id)?.ok_or_else(no_such_job)?;
        if self.running.contains_key(&id)
            || !matches!(job.state, JobState::Paused | JobState::Failed)
        {
            return Ok(());
        }
        self.resolved_again.remove(&id);
        self.requeue_resolved(job, probe)?;
        self.after_change();
        Ok(())
    }

    /// Queues the job to continue from the link `probe` resolved: from
    /// zero if the file changed on the server (noted as the job's error) or
    /// cannot be resumed. The job counts as checked against the server (see
    /// `Store::requeue_interrupted`). If the rest no longer fits on the disk,
    /// the job is Failed with that error, which is also returned; so is any
    /// error saving the job. The caller runs `after_change`.
    fn requeue_resolved(&mut self, mut job: JobRecord, probe: &Probe) -> Result<(), CoreError> {
        let id = job.id;
        // The link that served the file now: a confirm form may hand out a
        // fresh one.
        job.url = probe.requested_url.to_string();
        let change = change_on_server(&job, probe);
        let resumable = probe.ranges && probe.size.is_some();
        if change.is_some() || !job.resumable || !resumable {
            // Never stitch two versions of a file: start over.
            remove_if_exists(&partial_path(&job))?;
            self.store.replace_segments(id, &[])?;
            apply_probe(&mut job, probe);
        }
        if let Some(what) = &change {
            tracing::info!(job = id.0, "restarting from zero: {what}");
        }
        job.error = change.map(|what| (ErrorKind::FileChangedOnServer, what));
        job.state = JobState::Queued;
        let done = downloaded(&self.store, id)?;
        let fits = match job.total {
            Some(total) => check_space(
                &self.views,
                self.space,
                &job.dest_dir,
                total.saturating_sub(done),
                Some(total),
                Some(id),
            ),
            None => Ok(()),
        };
        if let Err(e) = &fits {
            job.state = JobState::Failed;
            job.error = Some((e.kind, e.detail.clone()));
        }
        self.store.update_job(&job)?;
        self.store.clear_revalidation(id)?;
        let view = view_of(&job, done);
        write(&self.views).insert(id, view.clone());
        self.emit(Event::JobUpdated(view));
        fits?;
        self.retry_counts.remove(&id);
        Ok(())
    }

    pub(super) async fn remove(&mut self, id: JobId, delete_files: bool) -> Result<(), CoreError> {
        if !read(&self.views).contains_key(&id) {
            return Err(no_such_job());
        }
        if let Some(run) = self.running.get(&id) {
            run.cancel.cancel();
            self.settle(&[id]).await;
        }
        self.cancel_retry(id);
        self.retry_counts.remove(&id);
        self.resolved_again.remove(&id);
        let removed = self.delete(id, delete_files);
        self.after_change();
        removed
    }

    fn delete(&mut self, id: JobId, delete_files: bool) -> Result<(), CoreError> {
        if delete_files && let Some(job) = self.store.get_job(id)? {
            delete_job_files(&self.store, &job)?;
        }
        self.store.delete_job(id)?;
        write(&self.views).remove(&id);
        self.emit(Event::JobRemoved(id));
        Ok(())
    }

    /// Refreshes the view from the store (finishing may have renamed the
    /// file) and announces it. A job that failed with an error that may pass
    /// (`is_transient`) waits to be retried instead, keeping the error; one
    /// whose link expired may have it resolved again (`resolve_again`).
    pub(super) fn on_finished(&mut self, id: JobId, outcome: JobOutcome) {
        let run = self.running.remove(&id);
        let mut view = load_view(&self.store, id).unwrap_or_else(|e| {
            tracing::warn!(job = id.0, "could not reload the finished job: {e}");
            read(&self.views).get(&id).cloned()
        });
        if let JobOutcome::Failed(e) = &outcome
            && e.kind == ErrorKind::LinkExpired
            && let Some(view) = &view
            && self.resolve_again(id, view.downloaded)
        {
            return;
        }
        let state = match outcome {
            JobOutcome::Completed(_) => {
                self.retry_counts.remove(&id);
                self.resolved_again.remove(&id);
                self.job_ended = true;
                JobState::Completed
            }
            // Interrupted by the shutdown, not paused by the user.
            JobOutcome::Paused if self.stopping => {
                if let Err(e) = self.store.requeue_interrupted(id) {
                    tracing::warn!(job = id.0, "could not record the interruption: {e}");
                }
                JobState::Queued
            }
            JobOutcome::Paused => JobState::Paused,
            JobOutcome::Failed(e) => {
                let state = if e.kind.is_transient() {
                    JobState::Waiting
                } else {
                    self.job_ended = true;
                    JobState::Failed
                };
                if let Some(view) = &mut view {
                    view.error = Some((e.kind, e.detail));
                }
                state
            }
        };
        if let Some(mut view) = view {
            if state == JobState::Waiting {
                if let Err(e) = self.store.set_state(id, JobState::Waiting) {
                    tracing::warn!(job = id.0, "could not record the wait: {e}");
                }
                // On shutdown, or without autostart, it stays Waiting.
                if !self.stopping && self.autostart {
                    let progressed =
                        run.is_some_and(|run| view.downloaded > run.downloaded_at_start);
                    self.retry_later(id, progressed);
                }
            }
            view.state = state;
            view.speed_bps = 0;
            write(&self.views).insert(id, view.clone());
            self.emit(Event::JobUpdated(view));
        }
        self.after_change();
    }

    /// A link that expired is resolved again from the link as added (spec
    /// 6.3), once until the job makes progress past this point or the user
    /// resumes it, so a link that keeps expiring fails. Runs like a
    /// download: the job stays Downloading, and pausing, removing or
    /// shutting down stops it. False if the job fails instead.
    fn resolve_again(&mut self, id: JobId, downloaded: u64) -> bool {
        if self.stopping
            || !self.autostart
            || self
                .resolved_again
                .get(&id)
                .is_some_and(|&at| downloaded <= at)
        {
            return false;
        }
        let job = match self.store.get_job(id) {
            Ok(Some(job)) => job,
            Ok(None) => return false,
            Err(e) => {
                tracing::warn!(
                    job = id.0,
                    "could not read the job to resolve it again: {e}"
                );
                return false;
            }
        };
        self.resolved_again.insert(id, downloaded);
        tracing::info!(
            job = id.0,
            "the link expired; resolving the link as added again"
        );
        self.spawn_resolve(job, downloaded);
        true
    }

    /// Resolves the job's link as added again in a task of its own, which
    /// sends `Resolved`. The job counts as running meanwhile, so pausing,
    /// removing or shutting down stops it.
    pub(super) fn spawn_resolve(&mut self, job: JobRecord, downloaded: u64) {
        let id = job.id;
        let cancel = CancellationToken::new();
        let stop = cancel.clone();
        let client = self.client.clone();
        let tx = self.job_tx.clone();
        tokio::spawn(async move {
            let url = fixed_url(&job.source_url);
            let probe = tokio::select! {
                _ = stop.cancelled() => None,
                probe = resolve(&client, &url, job.referer.as_deref()) => {
                    Some(probe.map(Box::new))
                }
            };
            let _ = tx.send(JobMsg::Resolved { id, probe });
        });
        self.running.insert(
            id,
            Run {
                cancel,
                downloaded_at_start: downloaded,
            },
        );
    }

    /// Queues the job whose link was resolved again, or fails it with the
    /// error of that resolve (or of queuing it). A job checked because it
    /// was interrupted goes on at once, in the slot it held; if the check
    /// failed with an error that may pass, it waits to be retried instead.
    pub(super) fn on_resolved(&mut self, id: JobId, probe: Option<Result<Box<Probe>, CoreError>>) {
        self.running.remove(&id);
        let job = match self.store.get_job(id) {
            Ok(Some(job)) => job,
            Ok(None) => return self.after_change(),
            Err(e) => return self.fail(id, e),
        };
        let revalidating = self.store.needs_revalidation(id).unwrap_or_else(|e| {
            tracing::warn!(job = id.0, "could not read whether the job is checked: {e}");
            false
        });
        let queued = match probe {
            // Stopped: it keeps the expired link's error, as a stopped run
            // would; a shutdown leaves it Queued, to be checked and run again.
            None => {
                let (state, stored) = if self.stopping {
                    (JobState::Queued, self.store.requeue_interrupted(id))
                } else {
                    (JobState::Paused, self.store.set_state(id, JobState::Paused))
                };
                if let Err(e) = stored {
                    tracing::warn!(job = id.0, "could not record the stop: {e}");
                }
                self.set_view_state(id, state);
                return self.after_change();
            }
            Some(Ok(probe)) => self.requeue_resolved(job, &probe),
            Some(Err(e)) => Err(e),
        };
        match queued {
            Ok(()) => {
                if revalidating && !self.stopping && self.state_of(id) == Some(JobState::Queued) {
                    self.start(id);
                }
                self.after_change();
            }
            Err(e) if revalidating && e.kind.is_transient() => self.wait_to_retry(id, e),
            Err(e) => self.fail(id, e),
        }
    }

    /// Records the job as Waiting with `e` and retries it later (only with
    /// `autostart`), as a run that failed with `e` would be.
    fn wait_to_retry(&mut self, id: JobId, e: CoreError) {
        self.record_error(id, JobState::Waiting, e);
        if !self.stopping && self.autostart {
            self.retry_later(id, false);
        }
        self.after_change();
    }

    /// Records the job as Failed with `e` and announces it.
    pub(super) fn fail(&mut self, id: JobId, e: CoreError) {
        self.record_failure(id, e);
        self.after_change();
    }

    /// `fail` without the `after_change`, for use within it.
    pub(super) fn record_failure(&mut self, id: JobId, e: CoreError) {
        self.record_error(id, JobState::Failed, e);
        self.job_ended = true;
    }

    /// Records the job in `state` with `e` as its error and announces it.
    fn record_error(&mut self, id: JobId, state: JobState, e: CoreError) {
        let error = Some((e.kind, e.detail));
        match self.store.get_job(id) {
            Ok(Some(mut job)) => {
                job.state = state;
                job.error = error.clone();
                if let Err(e) = self.store.update_job(&job) {
                    tracing::warn!(job = id.0, "could not record the error: {e}");
                }
            }
            Ok(None) => {}
            Err(e) => tracing::warn!(job = id.0, "could not record the error: {e}"),
        }
        let view = write(&self.views).get_mut(&id).map(|view| {
            view.state = state;
            view.speed_bps = 0;
            view.error = error;
            view.clone()
        });
        if let Some(view) = view {
            self.emit(Event::JobUpdated(view));
        }
    }
}

/// The pre-flight check: `remaining` more bytes of a `total`-byte file
/// must fit in `dir` beside what every Downloading job on the same disk
/// (other than `except`) still has to download.
pub(super) fn check_space(
    views: &Views,
    space: SpaceSource,
    dir: &Path,
    remaining: u64,
    total: Option<u64>,
    except: Option<JobId>,
) -> Result<(), CoreError> {
    let running: Vec<(PathBuf, u64)> = read(views)
        .values()
        .filter(|v| v.state == JobState::Downloading && Some(v.id) != except)
        .filter_map(|v| Some((v.dest_dir.clone(), v.total?.saturating_sub(v.downloaded))))
        .collect();
    let reserved = running
        .iter()
        .filter(|(other, _)| same_volume(other, dir))
        .fold(0u64, |sum, (_, left)| sum.saturating_add(*left));
    check_fits(space, dir, reserved, remaining, total)
}

/// The URL after the host's link fixups; an unparseable URL as is, for
/// `resolve` to report.
pub(super) fn fixed_url(url: &str) -> String {
    match url::Url::parse(url) {
        Ok(parsed) => fixups::rewrite(&parsed).unwrap_or(parsed).to_string(),
        Err(_) => url.to_owned(),
    }
}

/// The mirrors that serve the primary's file, each as the link to request
/// (a confirm form's, if one was followed). A mirror that fails to resolve,
/// serves another size, or whose validators say it serves another version
/// (the rule segment responses follow) is dropped.
pub(super) async fn vet_mirrors(
    client: &reqwest::Client,
    primary: &Probe,
    mirrors: &[String],
    referer: Option<&str>,
) -> Vec<String> {
    let mut kept = Vec::new();
    for mirror in mirrors {
        let why = match resolve(client, &fixed_url(mirror), referer).await {
            Err(e) => e.to_string(),
            Ok(probe) if probe.size != primary.size => format!(
                "size {} differs from {}",
                describe_size(probe.size),
                describe_size(primary.size)
            ),
            Ok(probe) => match mirror_change(primary, &probe) {
                Some(change) => change,
                None => {
                    kept.push(probe.requested_url.to_string());
                    continue;
                }
            },
        };
        tracing::info!(mirror = loggable(mirror), "dropping the mirror: {why}");
    }
    kept
}

/// How the mirror's validators differ from the primary's, as for a 206.
fn mirror_change(primary: &Probe, mirror: &Probe) -> Option<String> {
    validator_change(
        (primary.etag.as_deref(), primary.last_modified.as_deref()),
        (mirror.etag.as_deref(), mirror.last_modified.as_deref()),
    )
    .map(|what| format!("{what} (primary to mirror)"))
}

/// Copies what the probe learned into the record.
fn apply_probe(job: &mut JobRecord, probe: &Probe) {
    job.final_url = Some(probe.final_url.to_string());
    job.total = probe.size;
    job.resumable = probe.ranges && probe.size.is_some();
    job.etag = probe.etag.clone();
    job.last_modified = probe.last_modified.clone();
}

/// What says the file on the server is no longer the one partly downloaded:
/// the validators (see `validator_change`), or the size.
fn change_on_server(job: &JobRecord, probe: &Probe) -> Option<String> {
    let validator = validator_change(
        (job.etag.as_deref(), job.last_modified.as_deref()),
        (probe.etag.as_deref(), probe.last_modified.as_deref()),
    );
    validator.or_else(|| {
        (job.total != probe.size).then(|| {
            format!(
                "size changed from {} to {}",
                describe_size(job.total),
                describe_size(probe.size)
            )
        })
    })
}

fn describe_size(size: Option<u64>) -> String {
    size.map_or_else(|| "unknown".to_string(), |s| format!("{s} bytes"))
}

pub(super) fn view_of(job: &JobRecord, downloaded: u64) -> JobView {
    JobView {
        id: job.id,
        url: job.url.clone(),
        source_url: job.source_url.clone(),
        file_name: job.file_name.clone(),
        dest_dir: job.dest_dir.clone(),
        state: job.state,
        total: job.total,
        downloaded,
        speed_bps: 0,
        resumable: job.resumable,
        error: job.error.clone(),
    }
}

fn load_view(store: &Store, id: JobId) -> Result<Option<JobView>, CoreError> {
    let Some(job) = store.get_job(id)? else {
        return Ok(None);
    };
    Ok(Some(view_of(&job, downloaded(store, id)?)))
}

/// Durable progress: the sum of the stored segments' `done`.
pub(super) fn downloaded(store: &Store, id: JobId) -> Result<u64, CoreError> {
    Ok(store.load_segments(id)?.iter().map(|s| s.done).sum())
}

fn partial_path(job: &JobRecord) -> PathBuf {
    job.dest_dir
        .join(format!("{}.{PARTIAL_EXT}", job.file_name))
}

/// Deletes the job's partial file and, once completed, its downloaded file.
/// Spares both if another job is recorded under the same name, and spares a
/// downloaded file whose size is no longer the downloaded size.
fn delete_job_files(store: &Store, job: &JobRecord) -> Result<(), CoreError> {
    if store.name_used_by_other(job.id, &job.dest_dir, &job.file_name)? {
        return Ok(());
    }
    remove_if_exists(&partial_path(job))?;
    if job.state == JobState::Completed {
        let path = job.dest_dir.join(&job.file_name);
        let ours = std::fs::metadata(&path)
            .is_ok_and(|m| m.is_file() && job.total.is_none_or(|total| m.len() == total));
        if ours {
            remove_if_exists(&path)?;
        }
    }
    Ok(())
}

fn remove_if_exists(path: &Path) -> Result<(), CoreError> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(io_error(e)),
        _ => Ok(()),
    }
}
