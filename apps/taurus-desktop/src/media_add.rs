//! Video and audio links on the Add screen (Simple mode): Video / Audio
//! only, the quality, live streams and large playlists.
//!
//! Every link `Engine::inspect` reports as media is added with a
//! `MediaChoice`, the Simple default ("best") included: the engine sends a
//! web page that is not on its fast-path list to yt-dlp only then.
//!
//! A playlist of more than 50 videos: Download keeps the Add screen open
//! and adds it without confirmation; the engine answers NeedsConfirmation
//! ("playlist:<N>"), the screen asks, and "Download all"
//! (`playlist-confirmed`) adds it again, confirmed.

use std::path::Path;

use slint::ComponentHandle;
use taurus_core::media::{LiveStatus, MediaInfo};
use taurus_core::{CoreError, ErrorKind, JobView, MediaChoice};

use crate::add_flow::{PendingAdd, add_job};
use crate::app::{App, UiMsg};
use crate::detail::warn;
use crate::{AddVm, AppWindow, Messages, Screen, format, rows};

/// A playlist with more videos than this is added only once confirmed
/// (the engine's limit).
const LARGE_PLAYLIST: u32 = 50;

/// What is picked for a media link on the Add screen.
pub(crate) struct MediaPick {
    info: MediaInfo,
    audio: bool,
    /// A `QualityOption` id.
    quality: String,
    /// The playlist size the engine asked about: the screen asks too.
    asked: Option<u32>,
    /// An add went to the engine and has not answered yet.
    busy: bool,
}

impl MediaPick {
    pub(crate) fn new(info: MediaInfo) -> MediaPick {
        // A source without video (a song, a podcast) starts in Audio only.
        let audio = !info.qualities.is_empty() && info.qualities.iter().all(|q| q.audio_only);
        let mut pick = MediaPick {
            info,
            audio,
            quality: String::new(),
            asked: None,
            busy: false,
        };
        pick.quality = pick.ids()[0].to_owned();
        pick
    }

    /// The quality ids of the current mode, best first. Never empty: a
    /// mode the link lists nothing for offers its standard best.
    fn ids(&self) -> Vec<&str> {
        let ids: Vec<&str> = self
            .info
            .qualities
            .iter()
            .filter(|q| q.audio_only == self.audio)
            .map(|q| q.id.as_str())
            .collect();
        if ids.is_empty() {
            vec![if self.audio { "audio-best" } else { "best" }]
        } else {
            ids
        }
    }

    fn set_audio(&mut self, audio: bool) {
        if audio != self.audio {
            self.audio = audio;
            self.quality = self.ids()[0].to_owned();
        }
    }

    /// The next quality of the mode, back to the first after the last.
    fn cycle(&mut self) {
        let ids = self.ids();
        let next = ids
            .iter()
            .position(|id| *id == self.quality)
            .map_or(0, |i| (i + 1) % ids.len());
        self.quality = ids[next].to_owned();
    }

    /// The badge: "live" for a live or upcoming stream, else "music" or
    /// "media" by the mode.
    fn badge(&self) -> &'static str {
        match self.info.live {
            LiveStatus::IsLive | LiveStatus::IsUpcoming { .. } => "live",
            _ if self.audio => "music",
            _ => "media",
        }
    }

    /// The kind the destination is sorted by: Music for audio, else Videos.
    pub(crate) fn folder_kind(&self) -> &'static str {
        if self.audio { "music" } else { "media" }
    }

    /// The engine's choice. "Smaller file" is the `audio-small` quality,
    /// whose own arguments make a small MP3: no conversion on top (the MP3
    /// conversion's higher quality would win and make it larger).
    pub(crate) fn choice(&self) -> MediaChoice {
        MediaChoice {
            quality: self.quality.clone(),
            convert: None,
            ..MediaChoice::default()
        }
    }

    /// What `Engine::inspect` found, so the engine need not read the link
    /// again.
    pub(crate) fn info(&self) -> &MediaInfo {
        &self.info
    }

    fn large(&self) -> bool {
        self.info
            .playlist
            .as_ref()
            .is_some_and(|p| p.count > LARGE_PLAYLIST)
    }
}

/// The Add screen's media fields for `pick`, except the quality's label
/// and the folder (the window and the settings know those).
fn media_vm(pick: &MediaPick, now_unix: i64) -> AddVm {
    let info = &pick.info;
    let (live, starts_at) = match info.live {
        LiveStatus::IsLive => ("live", None),
        LiveStatus::IsUpcoming { starts_at } => (
            "upcoming",
            starts_at.and_then(|at| format::starts_at(at, now_unix, &chrono::Local)),
        ),
        _ => ("", None),
    };
    let size = info
        .qualities
        .iter()
        .find(|q| q.id == pick.quality)
        .and_then(|q| q.est_size);
    let listed = info.playlist.as_ref().map_or(0, |p| p.count);
    AddVm {
        is_media: true,
        kind: pick.badge().into(),
        name: info.title.as_str().into(),
        size_text: size.map(format::bytes).unwrap_or_default().into(),
        media_mode: if pick.audio { "audio" } else { "video" }.into(),
        quality_id: pick.quality.as_str().into(),
        duration_text: info
            .duration_secs
            .map(format::duration)
            .unwrap_or_default()
            .into(),
        uploader: info.uploader.as_deref().unwrap_or("").into(),
        live: live.into(),
        starts_at: starts_at.unwrap_or_default().into(),
        playlist_count: pick.asked.unwrap_or(listed).min(i32::MAX as u32) as i32,
        needs_confirm: pick.asked.is_some(),
        ..Default::default()
    }
}

/// The video's picture for the Add screen of a link already in the list;
/// empty (the screen shows a placeholder) when it cannot be read.
fn thumbnail(path: Option<&Path>) -> slint::Image {
    path.and_then(|p| slint::Image::load_from_path(p).ok())
        .unwrap_or_default()
}

/// The Add screen of a media link already in the list: its picture and
/// its row's badge.
pub(crate) fn duplicate_vm(view: &JobView, vm: AddVm) -> AddVm {
    let Some(media) = &view.media else {
        return vm;
    };
    AddVm {
        is_media: true,
        thumbnail: thumbnail(media.thumbnail.as_deref()),
        kind: rows::row_kind(view).into(),
        ..vm
    }
}

/// "playlist:51" → 51.
fn asked_count(detail: &str) -> Option<u32> {
    detail.strip_prefix("playlist:")?.trim().parse().ok()
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

impl App {
    /// `Engine::inspect` found a video, audio or stream: it goes on the
    /// Add screen with the Simple defaults.
    pub(crate) fn media_inspected(&self, ui: &AppWindow, p: &mut PendingAdd, info: MediaInfo) {
        let pick = MediaPick::new(info);
        p.kind = Some(pick.folder_kind());
        p.media = Some(pick);
        ui.set_add(self.media_shown(ui, p));
    }

    /// The Add screen for the pending media link as picked now.
    fn media_shown(&self, ui: &AppWindow, p: &PendingAdd) -> AddVm {
        let Some(pick) = &p.media else {
            return ui.get_add();
        };
        let vm = media_vm(pick, unix_now());
        AddVm {
            quality_label: ui
                .global::<Messages>()
                .invoke_quality(vm.quality_id.clone()),
            folder: self.destination_text(p, pick.folder_kind()),
            ..vm
        }
    }

    /// Changes the pending media pick with `change` and shows it.
    fn change_pick(&self, change: impl FnOnce(&mut MediaPick)) {
        let Some(ui) = self.window.upgrade() else {
            return;
        };
        let mut pending = self.pending.borrow_mut();
        let Some(p) = pending.as_mut() else {
            return;
        };
        let Some(pick) = p.media.as_mut().filter(|m| !m.busy) else {
            return;
        };
        change(pick);
        p.kind = Some(pick.folder_kind());
        ui.set_add(self.media_shown(&ui, p));
    }

    /// Video / Audio only: "video" | "audio".
    pub(crate) fn media_mode_changed(&self, mode: &str) {
        self.change_pick(|pick| pick.set_audio(mode == "audio"));
    }

    /// The next quality of the mode.
    pub(crate) fn quality_cycled(&self) {
        self.change_pick(MediaPick::cycle);
    }

    /// Download (or Record) of a media link. A large playlist stays on the
    /// Add screen until the engine says whether it needs confirming.
    pub(crate) fn media_confirmed(&self, ui: &AppWindow, mut p: PendingAdd, kind: &'static str) {
        let Some(pick) = p.media.as_mut() else {
            return self.hand_over(ui, p, kind, false);
        };
        if pick.busy {
            *self.pending.borrow_mut() = Some(p);
            return;
        }
        if !pick.large() || pick.asked.is_some() {
            let confirmed = pick.asked.is_some();
            return self.hand_over(ui, p, kind, confirmed);
        }
        pick.busy = true;
        let seq = p.seq;
        let (request, sorted) = self.request_for(&p, kind, false);
        *self.pending.borrow_mut() = Some(p);
        let engine = self.engine.clone();
        let tx = self.tx.clone();
        self.rt.spawn(async move {
            let result = add_job(engine, request, sorted).await.map(|_| ());
            tx.send(UiMsg::MediaAdded { seq, result });
        });
    }

    /// "Download all" of a large playlist: added, confirmed.
    pub(crate) fn playlist_confirmed(&self) {
        let Some(ui) = self.window.upgrade() else {
            return;
        };
        let Some(p) = self.pending.borrow_mut().take() else {
            return;
        };
        match (p.kind, &p.media) {
            (Some(kind), Some(pick)) if !pick.busy => self.hand_over(&ui, p, kind, true),
            _ => *self.pending.borrow_mut() = Some(p),
        }
    }

    /// The engine answered the unconfirmed add of a large playlist;
    /// ignored when the user moved on.
    pub(crate) fn media_added(&self, seq: u64, result: Result<(), CoreError>) {
        let Some(ui) = self.window.upgrade() else {
            return;
        };
        let mut pending = self.pending.borrow_mut();
        let Some(p) = pending.as_mut().filter(|p| p.seq == seq) else {
            return;
        };
        let Some(pick) = p.media.as_mut() else {
            return;
        };
        pick.busy = false;
        match result {
            // Added after all (the playlist shrank): on to the list.
            Ok(()) => {
                pending.take();
                drop(pending);
                ui.set_tab(0);
                ui.set_screen(Screen::Downloads);
                self.next_queued();
            }
            Err(e) if e.kind == ErrorKind::NeedsConfirmation => {
                let listed = pick.info.playlist.as_ref().map_or(0, |p| p.count);
                pick.asked = Some(asked_count(&e.detail).unwrap_or(listed));
                ui.set_add(self.media_shown(&ui, p));
            }
            Err(CoreError { kind, detail }) => {
                warn(format_args!("add failed: {detail}"));
                p.kind = None;
                ui.set_add(AddVm {
                    error_kind: kind.as_str().into(),
                    ..Default::default()
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use taurus_core::media::{PlaylistInfo, QualityOption};

    fn quality(id: &str, audio_only: bool, est_size: Option<u64>) -> QualityOption {
        QualityOption {
            id: id.into(),
            selector: String::new(),
            audio_only,
            est_size,
        }
    }

    fn info(qualities: Vec<QualityOption>) -> MediaInfo {
        MediaInfo {
            url: "https://www.youtube.com/watch?v=x".into(),
            title: "A Fine Video".into(),
            uploader: Some("Some Channel".into()),
            duration_secs: Some(212),
            thumbnail_url: None,
            extractor: "Youtube".into(),
            live: LiveStatus::NotLive,
            playlist: None,
            qualities,
            subtitle_languages: Vec::new(),
            auto_caption_languages: Vec::new(),
            all_qualities: Vec::new(),
        }
    }

    fn all_qualities() -> Vec<QualityOption> {
        vec![
            quality("best", false, Some(53_000_000)),
            quality("1080p", false, Some(53_000_000)),
            quality("720p", false, Some(23_000_000)),
            quality("audio-best", true, Some(3_000_000)),
            quality("audio-small", true, None),
        ]
    }

    #[test]
    fn modes_and_qualities_cycle() {
        let mut pick = MediaPick::new(info(all_qualities()));
        assert_eq!((pick.audio, pick.quality.as_str()), (false, "best"));
        assert_eq!(
            pick.choice(),
            MediaChoice {
                quality: "best".into(),
                ..MediaChoice::default()
            }
        );
        for want in ["1080p", "720p", "best"] {
            pick.cycle();
            assert_eq!(pick.quality, want);
        }
        pick.cycle();
        pick.set_audio(true);
        assert_eq!(pick.quality, "audio-best");
        assert_eq!((pick.badge(), pick.folder_kind()), ("music", "music"));
        pick.cycle();
        assert_eq!(
            pick.choice(),
            MediaChoice {
                quality: "audio-small".into(),
                ..MediaChoice::default()
            }
        );
        pick.cycle();
        assert_eq!(pick.quality, "audio-best");
        pick.set_audio(false);
        assert_eq!(
            (pick.quality.as_str(), pick.folder_kind()),
            ("best", "media")
        );

        // Nothing listed for a mode: its standard best.
        let mut pick = MediaPick::new(info(Vec::new()));
        assert_eq!(pick.quality, "best");
        pick.cycle();
        assert_eq!(pick.quality, "best");
        pick.set_audio(true);
        assert_eq!(pick.quality, "audio-best");
        // A source without video starts in Audio only.
        let songs = vec![
            quality("audio-best", true, None),
            quality("audio-small", true, None),
        ];
        assert!(MediaPick::new(info(songs)).audio);
    }

    #[test]
    fn the_add_screen_fields() {
        let now = 1_793_541_900;
        let pick = MediaPick::new(info(all_qualities()));
        let vm = media_vm(&pick, now);
        assert!(vm.is_media && !vm.needs_confirm);
        assert_eq!(
            (
                vm.kind.as_str(),
                vm.name.as_str(),
                vm.size_text.as_str(),
                vm.media_mode.as_str(),
                vm.quality_id.as_str(),
                vm.duration_text.as_str(),
                vm.uploader.as_str(),
                vm.live.as_str(),
            ),
            (
                "media",
                "A Fine Video",
                "53 MB",
                "video",
                "best",
                "3:32",
                "Some Channel",
                ""
            )
        );
        assert_eq!(vm.playlist_count, 0);

        let mut live = info(all_qualities());
        live.live = LiveStatus::IsLive;
        let vm = media_vm(&MediaPick::new(live), now);
        assert_eq!((vm.live.as_str(), vm.kind.as_str()), ("live", "live"));
        let mut upcoming = info(Vec::new());
        upcoming.live = LiveStatus::IsUpcoming { starts_at: None };
        let vm = media_vm(&MediaPick::new(upcoming), now);
        assert_eq!((vm.live.as_str(), vm.starts_at.as_str()), ("upcoming", ""));

        let mut list = info(Vec::new());
        list.playlist = Some(PlaylistInfo {
            title: "Big".into(),
            count: 51,
        });
        let mut pick = MediaPick::new(list);
        assert!(pick.large());
        assert_eq!(media_vm(&pick, now).playlist_count, 51);
        pick.asked = Some(52);
        let vm = media_vm(&pick, now);
        assert_eq!((vm.playlist_count, vm.needs_confirm), (52, true));
        pick.info.playlist.as_mut().unwrap().count = 50;
        assert!(!pick.large());
    }

    #[test]
    fn asked_counts() {
        assert_eq!(asked_count("playlist:51"), Some(51));
        assert_eq!(asked_count("playlist: 7"), Some(7));
        assert_eq!(asked_count("something else"), None);
        assert_eq!(asked_count("playlist:many"), None);
    }

    #[test]
    fn unreadable_pictures_are_placeholders() {
        assert_eq!(thumbnail(None).size().width, 0);
        let missing = std::env::temp_dir().join("hyades-no-such-thumb.jpg");
        assert_eq!(thumbnail(Some(&missing)).size().width, 0);
    }
}
