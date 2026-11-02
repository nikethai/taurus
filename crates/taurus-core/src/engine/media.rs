//! Media jobs in the engine: adding a video, audio or playlist link (read
//! by yt-dlp first), starting its runner (`media_job`), following what it
//! reports, stopping a live recording, and its thumbnail.

use std::path::{Path, PathBuf};

use tokio_util::sync::CancellationToken;

use crate::errors::{CoreError, ErrorKind};
use crate::filename::sanitize;
use crate::http::JobOutcome;
use crate::media::run::MediaPhase;
use crate::media::thumbnail::{THUMBS_DIR, fetch_thumbnail, thumbnail_in};
use crate::media::{
    Container, ConvertPreset, LiveStatus, MediaInfo, MediaOptions, SubtitleChoice,
    own_partial_files, standard_quality, validate_subtitles,
};
use crate::store::{JobId, JobKind, JobRecord, JobState, NewJob};

use super::actor::{Actor, Command, JobMsg, Run};
use super::inspect::clean_browser;
use super::jobs::view_of;
use super::media_job::{MediaJob, run_media_job};
use super::media_rate::MediaRate;
use super::schedule::check_space;
use super::{AddRequest, Engine, Event, JobView, no_such_job, read, write};

/// Playlists longer than this need `AddRequest::confirm_large_playlist`.
const LARGE_PLAYLIST: u32 = 50;

/// How to download a media link: one of the link's
/// [`QualityOption`](crate::media::QualityOption) ids (`best`, `1080p`,
/// `720p`, `audio-best`, `audio-small`, or a `<height>p` the link lists
/// in `MediaInfo::all_qualities`), an optional conversion, and what only a
/// video takes: the browser whose cookies to sign in with, subtitles and
/// the container.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MediaChoice {
    pub quality: String,
    pub convert: Option<ConvertPreset>,
    /// `chrome`, `firefox`, ... optionally `:<profile>`
    /// ([`validate_browser`](crate::media::validate_browser)); an empty
    /// string is none.
    pub cookies_from_browser: Option<String>,
    pub subtitles: Option<SubtitleChoice>,
    /// `None` is MP4; audio ignores it. The smaller-file conversion makes
    /// an MP4, so it is refused with Mkv or Webm.
    pub container: Option<Container>,
}

/// What only a media job has.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct MediaView {
    pub phase: MediaPhase,
    /// Only the audio is kept (an audio quality, or the MP3 conversion).
    pub audio_only: bool,
    /// The link was live or upcoming when added: it is recorded.
    pub live: bool,
    /// Playlist entries finished, of `entries_total` (1 of 1 once a single
    /// video is done; 0 of 0 before anything is known).
    pub entries_done: u32,
    pub entries_total: u32,
    /// How long the live stream has been recorded, while it is.
    pub live_elapsed_secs: Option<u64>,
    /// When an upcoming stream begins (unix seconds), as inspected.
    pub starts_at: Option<i64>,
    /// The video's picture, under `data_dir/thumbs`, once downloaded.
    pub thumbnail: Option<PathBuf>,
    /// The finished files, for "Open" (a playlist has one per entry).
    pub files: Vec<PathBuf>,
}

/// A change of a running media job's view, from its runner.
#[doc(hidden)]
#[derive(Debug)]
pub struct MediaUpdate {
    pub phase: MediaPhase,
    pub entries_done: u32,
    pub entries_total: u32,
    pub files: Vec<PathBuf>,
    pub live_elapsed_secs: Option<u64>,
    /// Whether front ends should hear of it (`Event::JobUpdated`): set
    /// unless only the elapsed time moved.
    pub announce: bool,
}

/// A media job to add, as read in the caller's task.
pub(super) struct MediaAdd {
    pub(super) name: String,
    pub(super) options: MediaOptions,
    pub(super) starts_at: Option<i64>,
}

impl Engine {
    /// `add` for a media link, once `req.dest_dir` is checked.
    pub(super) async fn add_media(&self, mut req: AddRequest) -> Result<JobId, CoreError> {
        if !req.mirrors.is_empty() || req.checksum.is_some() {
            return Err(CoreError {
                kind: ErrorKind::Unknown,
                detail: "mirrors and checksum apply only to web downloads".to_string(),
            });
        }
        // A bad choice is refused before anything is read.
        let cookies = check_choice(req.media.as_ref())?;
        // Read once: the front end usually inspected the link already.
        let info = match req.media_info.take() {
            Some(info) => info,
            None => self.media_info(req.url.trim(), cookies.as_deref()).await?,
        };
        if let Some(playlist) = &info.playlist
            && playlist.count > LARGE_PLAYLIST
            && !req.confirm_large_playlist
        {
            return Err(CoreError {
                kind: ErrorKind::NeedsConfirmation,
                detail: format!("playlist:{}", playlist.count),
            });
        }
        let options = media_options(&info, req.media.as_ref())?;
        // A single video's estimated size must fit, as a file's must.
        let est_size = info
            .qualities
            .iter()
            .chain(&info.all_qualities)
            .find(|q| q.id == options.quality)
            .and_then(|q| q.est_size)
            .filter(|_| !options.playlist && !options.live_from_start);
        if let Some(size) = est_size {
            check_space(
                &self.views,
                self.space,
                &req.dest_dir,
                size,
                Some(size),
                None,
            )?;
        }
        let starts_at = match info.live {
            LiveStatus::IsUpcoming { starts_at } => starts_at,
            _ => None,
        };
        let add = MediaAdd {
            name: sanitize(&info.title),
            options,
            starts_at,
        };
        let id = self
            .call(|reply| Command::AddMedia { req, add, reply })
            .await??;
        if let Some(url) = info.thumbnail_url {
            let client = self.client.get();
            let dir = self.data_dir.join(THUMBS_DIR);
            let tx = self.hold_tx.clone();
            tokio::spawn(async move {
                match fetch_thumbnail(&client, &url, &dir, &id.0.to_string()).await {
                    Ok(path) => {
                        let _ = tx.send(JobMsg::Thumbnail { id, path });
                    }
                    Err(e) => tracing::info!(job = id.0, "no thumbnail: {e}"),
                }
            });
        }
        Ok(id)
    }

    /// Stops a live recording (or any running media download) gracefully
    /// and returns once it is Completed with what was recorded: yt-dlp is
    /// interrupted as by Ctrl+C (killed if it has not stopped within
    /// `Timing::media_stop_grace`: 10 s, 1 s on Windows), and a recording
    /// it left unfinished is made playable with ffmpeg (its video and audio
    /// parts joined into one MP4). One stopped before anything was
    /// recorded (its tools still being installed, the stream not begun, or
    /// yt-dlp being updated after the site broke) is Paused instead, without
    /// an error, to be started again or removed. A media job that is not
    /// running is left as it is; any other job is refused.
    pub async fn stop_recording(&self, id: JobId) -> Result<(), CoreError> {
        // The actor only asks the run to stop; the end is waited for here,
        // so other commands are served meanwhile.
        let stopping = self
            .call(|reply| Command::StopRecording { id, reply })
            .await??;
        if stopping {
            self.wait(id).await;
        }
        Ok(())
    }
}

/// Refuses a choice whose video options cannot be used (an unknown browser,
/// bad subtitle languages, the smaller-file conversion with Mkv or Webm,
/// which it would turn into MP4); returns the browser, trimmed (none if
/// empty).
fn check_choice(choice: Option<&MediaChoice>) -> Result<Option<String>, CoreError> {
    let Some(choice) = choice else {
        return Ok(None);
    };
    let cookies = clean_browser(choice.cookies_from_browser.as_deref())?;
    if choice.convert == Some(ConvertPreset::SmallerMp4)
        && matches!(choice.container, Some(Container::Mkv | Container::Webm))
    {
        return Err(CoreError {
            kind: ErrorKind::Unknown,
            detail: "the smaller-file conversion always makes an MP4".to_owned(),
        });
    }
    if let Some(subtitles) = &choice.subtitles {
        validate_subtitles(subtitles)?;
    }
    Ok(cookies.map(str::to_owned))
}

/// The stored options for `info` downloaded as `choice` (default `best`).
/// A quality the link does not list, but which is one of the five, is used
/// as it is usually built (a playlist lists only `best` and `audio-best`).
fn media_options(
    info: &MediaInfo,
    choice: Option<&MediaChoice>,
) -> Result<MediaOptions, CoreError> {
    let id = choice
        .map(|c| c.quality.trim())
        .filter(|q| !q.is_empty())
        .unwrap_or("best");
    let quality = info
        .qualities
        .iter()
        .chain(&info.all_qualities)
        .find(|q| q.id == id)
        .cloned()
        .or_else(|| standard_quality(id))
        .ok_or_else(|| CoreError {
            kind: ErrorKind::Unknown,
            detail: format!("unknown quality {id:?}"),
        })?;
    let cookies_from_browser = choice
        .and_then(|c| c.cookies_from_browser.as_deref())
        .map(str::trim)
        .filter(|b| !b.is_empty());
    let container = choice.and_then(|c| c.container);
    let subtitles = choice.and_then(|c| c.subtitles.clone());
    Ok(MediaOptions {
        quality: quality.id,
        selector: quality.selector,
        audio_only: quality.audio_only,
        convert: choice.and_then(|c| c.convert),
        playlist: info.playlist.is_some(),
        live_from_start: matches!(
            info.live,
            LiveStatus::IsLive | LiveStatus::IsUpcoming { .. }
        ),
        partial_files: Vec::new(),
        cookies_from_browser: cookies_from_browser.map(str::to_owned),
        subtitles,
        container,
    })
}

impl Actor {
    /// Queues the media job: named after the video or playlist (the name
    /// becomes the finished file's, or the playlist folder's).
    pub(super) fn add_media(&mut self, req: AddRequest, add: MediaAdd) -> Result<JobId, CoreError> {
        let url = req.url.trim().to_owned();
        let id = self.store.insert_job(&NewJob {
            url: url.clone(),
            source_url: req.url,
            dest_dir: req.dest_dir,
            file_name: add.name,
            connections: 1,
            mirrors: Vec::new(),
            checksum: None,
            referer: req.referer,
            queue_pos: 0,
            kind: JobKind::Media,
            info_hash: None,
            torrent_meta: None,
            only_files: None,
        })?;
        let stored = self.store.get_job(id).and_then(|job| {
            let mut job = job.ok_or_else(no_such_job)?;
            job.media = Some(add.options);
            self.store.update_job(&job)?;
            Ok(job)
        });
        let job = match stored {
            Ok(job) => job,
            Err(e) => {
                if let Err(cleanup) = self.store.delete_job(id) {
                    tracing::warn!(job = id.0, "could not remove a half-added job: {cleanup}");
                }
                return Err(e);
            }
        };
        let mut view = view_of(&self.store, &job, 0);
        if let Some(media) = &mut view.media {
            media.starts_at = add.starts_at;
        }
        write(&self.views).insert(id, view.clone());
        self.emit(Event::JobAdded(view));
        self.after_change();
        Ok(id)
    }

    /// Spawns the media job's runner; the job is Downloading (Recording for
    /// a live stream) from here on, and holds a `max_active` slot.
    pub(super) fn start_media(&mut self, id: JobId) {
        let job = match self.store.get_job(id) {
            Ok(Some(job)) if job.media.is_some() => job,
            Ok(_) => {
                return self.fail(
                    id,
                    CoreError {
                        kind: ErrorKind::Unknown,
                        detail: "the media job has no options".to_string(),
                    },
                );
            }
            Err(e) => return self.fail(id, e),
        };
        let live = job.media.as_ref().is_some_and(|m| m.live_from_start);
        let state = if live {
            JobState::Recording
        } else {
            JobState::Downloading
        };
        if let Err(e) = self.store.set_state(id, state) {
            tracing::warn!(job = id.0, "could not record the start: {e}");
        }
        let downloaded_at_start = read(&self.views).get(&id).map_or(0, |v| v.downloaded);
        let cancel = CancellationToken::new();
        let finish = CancellationToken::new();
        let (media_rate, rate) = MediaRate::new(self.media_rate());
        // A recording keeps the limit it started with.
        if !live {
            self.media_rates.insert(id, media_rate);
        }
        let ctx = MediaJob {
            job,
            tools: self.tools.clone(),
            store: self.store.clone(),
            grace: self.timing.media_stop_grace,
            rate,
            proxy: self.settings.network.proxy.clone(),
            events: self.events.clone(),
            cancel: cancel.clone(),
            finish: finish.clone(),
            msgs: self.job_tx.clone(),
        };
        let tx = self.job_tx.clone();
        tokio::spawn(async move {
            // A task of its own, so a panic still ends in `Finished`; the
            // runner's messages all precede it.
            let outcome = tokio::spawn(run_media_job(ctx)).await.unwrap_or_else(|e| {
                JobOutcome::Failed(CoreError {
                    kind: ErrorKind::Unknown,
                    detail: format!("media task crashed: {e}"),
                })
            });
            let _ = tx.send(JobMsg::Finished { id, outcome });
        });
        self.media_stops.insert(id, finish);
        self.running.insert(
            id,
            Run {
                cancel,
                downloaded_at_start,
            },
        );
        self.set_view_state(id, state);
    }

    /// Asks the running media job to stop as a finished recording; true if
    /// it runs (its end is then to be waited for).
    pub(super) fn stop_recording(&mut self, id: JobId) -> Result<bool, CoreError> {
        let kind = read(&self.views).get(&id).map(|v| v.kind);
        match kind {
            None => return Err(no_such_job()),
            Some(JobKind::Media) => {}
            Some(_) => {
                return Err(CoreError {
                    kind: ErrorKind::Unknown,
                    detail: "only a video or live recording can be stopped this way".to_string(),
                });
            }
        }
        match self.media_stops.get(&id) {
            Some(finish) if self.running.contains_key(&id) => {
                finish.cancel();
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    pub(super) fn on_media_update(&mut self, id: JobId, update: MediaUpdate) {
        if !self.running.contains_key(&id) {
            return;
        }
        let view = write(&self.views).get_mut(&id).and_then(|view| {
            let media = view.media.as_mut()?;
            media.phase = update.phase;
            media.entries_done = update.entries_done;
            media.entries_total = update.entries_total;
            media.files = update.files;
            media.live_elapsed_secs = update.live_elapsed_secs;
            update.announce.then(|| view.clone())
        });
        if let Some(view) = view {
            self.emit(Event::JobUpdated(view));
        }
        // A speed-limit change may have waited for the download to go on.
        self.apply_wanted(id, update.phase);
    }

    pub(super) fn on_thumbnail(&mut self, id: JobId, path: PathBuf) {
        let view = write(&self.views).get_mut(&id).and_then(|view| {
            view.media.as_mut()?.thumbnail = Some(path.clone());
            Some(view.clone())
        });
        match view {
            Some(view) => self.emit(Event::JobUpdated(view)),
            // Removed meanwhile.
            None => {
                let _ = std::fs::remove_file(&path);
            }
        }
    }

    /// Whether the job is a media job, as its view says.
    pub(super) fn is_media(&self, id: JobId) -> bool {
        read(&self.views)
            .get(&id)
            .is_some_and(|v| v.kind == JobKind::Media)
    }

    /// A media view reloaded from the store keeps what only its run knew:
    /// the progress short of Completed (yt-dlp's partial files are not
    /// counted in the store) and the runner's phase, entries and files.
    pub(super) fn keep_media_progress(&self, view: &mut JobView) {
        if view.kind != JobKind::Media || view.state == JobState::Completed {
            return;
        }
        if let Some(before) = read(&self.views).get(&view.id) {
            view.downloaded = before.downloaded;
            view.total = view.total.or(before.total);
        }
    }

    /// Deletes what the removed media job leaves: its thumbnail, and with
    /// `delete_files` the partial files its runs recorded (see
    /// `own_partial_files`), chosen by the job, never by a name; with
    /// `finished_too` (`RemoveFiles::All`) also those it finished (its
    /// view's, a single video's file once Completed). Without it a finished
    /// file is never deleted, even one of a paused playlist. A playlist's
    /// folder is kept.
    pub(super) fn delete_media(&self, job: &JobRecord, delete_files: bool, finished_too: bool) {
        let (thumbnail, mut finished) = read(&self.views)
            .get(&job.id)
            .and_then(|v| v.media.as_ref())
            .map(|m| (m.thumbnail.clone(), m.files.clone()))
            .unwrap_or_default();
        if let Some(path) = thumbnail {
            let _ = std::fs::remove_file(path);
        }
        if !delete_files {
            return;
        }
        let single = job.media.as_ref().is_some_and(|m| !m.playlist);
        if single && job.state == JobState::Completed {
            finished.push(job.dest_dir.join(&job.file_name));
        }
        let recorded = job
            .media
            .as_ref()
            .map_or(&[][..], |m| m.partial_files.as_slice());
        let mut files = own_partial_files(recorded, &finished);
        if finished_too {
            files.extend(finished);
        }
        for path in files {
            if path.is_file()
                && let Err(e) = std::fs::remove_file(&path)
            {
                tracing::warn!(job = job.id.0, "could not delete {}: {e}", path.display());
            }
        }
    }
}

/// What only a media job's view has, as stored: its phase as it would
/// start, nothing counted yet; a finished single video's file; and what
/// `known` (the job's view so far) has that the store does not.
pub(super) fn stored_media_view(job: &JobRecord, known: Option<&MediaView>) -> Option<MediaView> {
    let options = job.media.as_ref()?;
    if let Some(known) = known {
        return Some(known.clone());
    }
    let done = job.state == JobState::Completed;
    let files = if done && !options.playlist {
        vec![job.dest_dir.join(&job.file_name)]
    } else {
        Vec::new()
    };
    Some(MediaView {
        audio_only: options.audio_only || options.convert == Some(ConvertPreset::Mp3),
        live: options.live_from_start,
        phase: if options.live_from_start && !done {
            MediaPhase::WaitingForStream
        } else {
            MediaPhase::Downloading
        },
        entries_done: u32::from(done),
        entries_total: u32::from(done),
        live_elapsed_secs: None,
        starts_at: None,
        thumbnail: None,
        files,
    })
}

/// Sets each restored media view's thumbnail, if one was downloaded.
pub(super) fn restore_thumbnails<'a>(
    data_dir: &Path,
    views: impl Iterator<Item = &'a mut JobView>,
) {
    let dir = data_dir.join(THUMBS_DIR);
    for view in views {
        let id = view.id.0.to_string();
        if let Some(media) = &mut view.media {
            media.thumbnail = thumbnail_in(&dir, &id);
        }
    }
}
