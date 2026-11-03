//! Adding a download: a link arrives (pasted, dropped, forwarded by a
//! second launch, or offered from the clipboard), the Add screen shows what
//! it is, and Download hands it to the engine.
//!
//! Forwarded batches: the first link goes to the Add screen; the others
//! wait in a queue and come up one by one as each Add screen is confirmed
//! or closed. A link that arrives while the Add screen is open waits too,
//! except one pasted in the window, which replaces it.

use std::path::{Path, PathBuf};

use slint::ComponentHandle;
use taurus_core::{AddRequest, CoreError, Engine, ErrorKind, JobId, JobKind, LinkInfo};

use crate::adv_pick::AdvPick;
use crate::app::{App, UiMsg};
use crate::detail::warn;
use crate::media_add::{self, MediaPick};
use crate::{AddVm, AppWindow, BannerVm, Screen, format, rows};

/// The link on the Add screen.
pub(crate) struct PendingAdd {
    pub(crate) seq: u64,
    pub(crate) url: String,
    /// The kind the destination is sorted by, once the link was looked at
    /// without error.
    pub(crate) kind: Option<&'static str>,
    /// A folder picked on the Add screen for this download only.
    folder: Option<PathBuf>,
    /// What is picked for a video or audio link.
    pub(crate) media: Option<MediaPick>,
    /// What is picked on the Advanced Add screen (`adv_add.rs`).
    pub(crate) adv: Option<AdvPick>,
    /// The browser whose cookies the link was last read with ("" none).
    pub(crate) adv_cookies: String,
}

impl PendingAdd {
    fn new(seq: u64, url: String) -> PendingAdd {
        PendingAdd {
            seq,
            url,
            kind: None,
            folder: None,
            media: None,
            adv: None,
            adv_cookies: String::new(),
        }
    }
}

/// The type subfolder a kind is sorted into when "sort by type" is on;
/// `None` saves it in the folder itself.
pub fn sort_folder(kind: &str) -> Option<&'static str> {
    match kind {
        "media" | "live" => Some("Videos"),
        "music" => Some("Music"),
        "image" => Some("Pictures"),
        "file" => Some("Documents"),
        _ => None,
    }
}

/// The OS Downloads folder, else the home folder, else the current one.
pub fn default_download_dir() -> PathBuf {
    let dirs = directories::UserDirs::new();
    dirs.as_ref()
        .and_then(|d| d.download_dir().map(Path::to_path_buf))
        .or_else(|| dirs.as_ref().map(|d| d.home_dir().to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Where a download of `kind` goes: the folder picked for it, else the
/// settings folder (or the OS Downloads folder) with its type subfolder
/// when `sort_by_type`.
fn destination(
    picked: Option<&Path>,
    folder: Option<&Path>,
    sort_by_type: bool,
    kind: &str,
) -> PathBuf {
    if let Some(p) = picked {
        return p.to_path_buf();
    }
    let base = folder.map_or_else(default_download_dir, Path::to_path_buf);
    match sort_folder(kind) {
        Some(sub) if sort_by_type => base.join(sub),
        _ => base,
    }
}

fn folder_text(folder: Option<&Path>) -> slint::SharedString {
    folder
        .map(|p| p.display().to_string())
        .unwrap_or_default()
        .into()
}

/// What the Add screen shows for a file or a torrent; `None` for a video
/// link (see `media_add.rs`).
fn add_vm(info: &LinkInfo) -> Option<(AddVm, &'static str)> {
    let shown = match info {
        LinkInfo::File { name, size, .. } => {
            let kind = rows::kind_for(name, JobKind::Http);
            let vm = AddVm {
                kind: kind.into(),
                name: name.as_str().into(),
                size_text: size.map(format::bytes).unwrap_or_default().into(),
                ..Default::default()
            };
            (vm, kind)
        }
        LinkInfo::Media(_) => return None,
        LinkInfo::Torrent(t) => {
            let kind = rows::kind_for(&t.name, JobKind::Torrent);
            let vm = AddVm {
                kind: kind.into(),
                name: t.name.as_str().into(),
                size_text: format::bytes(t.total).into(),
                is_torrent: true,
                files_text: t.files.len().to_string().into(),
                ..Default::default()
            };
            (vm, kind)
        }
    };
    Some(shown)
}

impl App {
    /// A link typed, pasted or dropped in the window: shows it on the Add
    /// screen (already in the list, or being looked at).
    pub(crate) fn link_entered(&self, text: &str) {
        let url = text.trim().to_string();
        let Some(ui) = self.window.upgrade() else {
            return;
        };
        if url.is_empty() {
            return;
        }
        let seq = self.seq.get() + 1;
        self.seq.set(seq);
        let folder = self.settings.borrow().ui.folder.clone();
        *self.pending.borrow_mut() = Some(PendingAdd::new(seq, url.clone()));
        // A web page: Advanced offers "Find files →" (grab links) for it.
        let page = crate::batch::line_kind(&url) == crate::batch::LineKind::Page;
        ui.set_adv_page_url(if page { url.as_str().into() } else { "".into() });
        if let Some(id) = self.engine.find_by_url(&url) {
            let view = self.view.borrow();
            let existing = view.view(id);
            let vm = AddVm {
                duplicate: true,
                kind: existing.map_or("file", rows::row_kind).into(),
                name: existing.map(|v| v.file_name.clone()).unwrap_or(url).into(),
                size_text: existing
                    .and_then(|v| v.total)
                    .map(format::bytes)
                    .unwrap_or_default()
                    .into(),
                folder: existing
                    .map(|v| folder_text(Some(&v.dest_dir)))
                    .unwrap_or_default(),
                ..Default::default()
            };
            let vm = match existing {
                Some(v) => media_add::duplicate_vm(v, vm),
                None => vm,
            };
            self.adv_duplicate(&ui, &vm);
            ui.set_add(vm);
            ui.set_screen(Screen::Add);
            return;
        }
        ui.set_add(AddVm {
            loading: true,
            folder: folder_text(folder.as_deref()),
            ..Default::default()
        });
        self.adv_loading(&ui, folder_text(folder.as_deref()));
        ui.set_screen(Screen::Add);
        let engine = self.engine.clone();
        let tx = self.tx.clone();
        self.rt.spawn(async move {
            let result = engine.inspect(&url).await.map_err(|e| e.kind);
            tx.send(UiMsg::Inspected {
                seq,
                result: Box::new(result),
            });
        });
    }

    /// `Engine::inspect` answered; ignored when the user moved on.
    pub(crate) fn inspected(&self, seq: u64, result: Result<LinkInfo, ErrorKind>) {
        let Some(ui) = self.window.upgrade() else {
            return;
        };
        let mut pending = self.pending.borrow_mut();
        let Some(p) = pending.as_mut().filter(|p| p.seq == seq) else {
            return;
        };
        // The Advanced screen first: the Simple one takes the result.
        self.adv_inspected(&ui, p, &result);
        let result = match result {
            Ok(LinkInfo::Media(info)) => return self.media_inspected(&ui, p, info),
            other => other,
        };
        let shown = result.and_then(|info| add_vm(&info).ok_or(ErrorKind::SiteNotSupported));
        let vm = match shown {
            Ok((vm, kind)) => {
                p.kind = Some(kind);
                AddVm {
                    folder: self.destination_text(p, kind),
                    ..vm
                }
            }
            Err(kind) => AddVm {
                error_kind: kind.as_str().into(),
                ..Default::default()
            },
        };
        ui.set_add(vm);
    }

    /// Where the pending download of `kind` goes, and whether that is a
    /// type subfolder (made when missing).
    pub(crate) fn destination_of(&self, p: &PendingAdd, kind: &str) -> (PathBuf, bool) {
        let s = self.settings.borrow();
        let dest = destination(
            p.folder.as_deref(),
            s.ui.folder.as_deref(),
            s.ui.sort_by_type,
            kind,
        );
        let sorted = p.folder.is_none() && s.ui.sort_by_type && sort_folder(kind).is_some();
        (dest, sorted)
    }

    /// Where a download of `kind` goes when no folder was picked for it,
    /// and whether that is a type subfolder (made when missing).
    pub(crate) fn default_destination(&self, kind: &str) -> (PathBuf, bool) {
        let s = self.settings.borrow();
        let dest = destination(None, s.ui.folder.as_deref(), s.ui.sort_by_type, kind);
        (dest, s.ui.sort_by_type && sort_folder(kind).is_some())
    }

    /// "Save to" on the Add screen: the real destination, type subfolder
    /// included; "" (the screen says "Downloads folder") when that is the
    /// OS Downloads folder itself.
    pub(crate) fn destination_text(&self, p: &PendingAdd, kind: &str) -> slint::SharedString {
        let (dest, sorted) = self.destination_of(p, kind);
        let plain_default =
            p.folder.is_none() && !sorted && self.settings.borrow().ui.folder.is_none();
        if plain_default {
            return "".into();
        }
        folder_text(Some(&dest))
    }

    /// Download: the engine adds the link; the list shows it.
    pub(crate) fn add_confirmed(&self) {
        let Some(ui) = self.window.upgrade() else {
            return;
        };
        let Some(p) = self.pending.borrow_mut().take() else {
            return;
        };
        let Some(kind) = p.kind else {
            // Still being looked at, or it failed: nothing to add.
            *self.pending.borrow_mut() = Some(p);
            return;
        };
        if p.media.is_some() {
            return self.media_confirmed(&ui, p, kind);
        }
        self.hand_over(&ui, p, kind, false);
    }

    /// The engine's request for the pending add of `kind`, and whether its
    /// folder is a type subfolder. A media link carries its choice.
    pub(crate) fn request_for(
        &self,
        p: &PendingAdd,
        kind: &str,
        confirm_large_playlist: bool,
    ) -> (AddRequest, bool) {
        let (dest_dir, sorted) = self.destination_of(p, kind);
        let request = AddRequest {
            url: p.url.clone(),
            dest_dir,
            media: p.media.as_ref().map(MediaPick::choice),
            media_info: p.media.as_ref().map(|m| m.info().clone()),
            confirm_large_playlist,
            ..Default::default()
        };
        (request, sorted)
    }

    /// Hands the pending add to the engine and shows the list; a refusal
    /// comes back on the Add screen.
    pub(crate) fn hand_over(
        &self,
        ui: &AppWindow,
        p: PendingAdd,
        kind: &str,
        confirm_large_playlist: bool,
    ) {
        let (request, sorted) = self.request_for(&p, kind, confirm_large_playlist);
        ui.set_tab(0);
        ui.set_screen(Screen::Downloads);
        let engine = self.engine.clone();
        let tx = self.tx.clone();
        self.rt.spawn(async move {
            if let Err(CoreError { kind, detail }) = add_job(engine, request, sorted).await {
                warn(format_args!("add failed: {detail}"));
                tx.send(UiMsg::AddFailed(kind));
            }
        });
        self.next_queued();
    }

    /// Back, Cancel, Close or "Show it" on the Add screen.
    pub(crate) fn add_cancelled(&self) {
        self.pending.borrow_mut().take();
        self.next_queued();
    }

    /// The engine refused a confirmed add: say why on the Add screen,
    /// unless another link is there already.
    pub(crate) fn add_failed(&self, kind: ErrorKind) {
        let Some(ui) = self.window.upgrade() else {
            return;
        };
        if self.pending.borrow().is_some() {
            return;
        }
        // An empty pending add, so Close goes on with the queue.
        let seq = self.seq.get() + 1;
        self.seq.set(seq);
        *self.pending.borrow_mut() = Some(PendingAdd::new(seq, String::new()));
        ui.set_adv_page_url("".into());
        ui.set_add(AddVm {
            error_kind: kind.as_str().into(),
            ..Default::default()
        });
        self.adv_error(&ui, kind);
        ui.set_screen(Screen::Add);
    }

    /// The next queued link, on the next tick (after the window finished
    /// its own navigation for this click).
    pub(crate) fn next_queued(&self) {
        if let Some(link) = self.queue.borrow_mut().pop_front() {
            self.tx.send(UiMsg::Link(link));
        }
    }

    /// A link from outside the window: now, or after the open Add screen.
    pub(crate) fn offer_link(&self, link: String) {
        if self.pending.borrow().is_some() {
            self.queue.borrow_mut().push_back(link);
        } else {
            self.link_entered(&link);
        }
    }

    /// Links from a second launch: the window is shown and the first
    /// link is looked at; the others wait their turn.
    pub(crate) fn forwarded(&self, links: Vec<String>) {
        self.show_window();
        let mut links = links.into_iter();
        if let Some(first) = links.next() {
            self.offer_link(first);
        }
        self.queue.borrow_mut().extend(links);
    }

    /// Picks a folder: for this download on the Add screen, else the
    /// download folder in the settings.
    pub(crate) fn choose_folder(&self) {
        let Some(ui) = self.window.upgrade() else {
            return;
        };
        let current = self.settings.borrow().ui.folder.clone();
        let start = current.clone().unwrap_or_else(default_download_dir);
        let Some(picked) = rfd::FileDialog::new().set_directory(&start).pick_folder() else {
            return;
        };
        if ui.get_screen() == Screen::Add
            && let Some(p) = self.pending.borrow_mut().as_mut()
        {
            ui.set_add(AddVm {
                folder: folder_text(Some(&picked)),
                ..ui.get_add()
            });
            p.folder = Some(picked);
            self.render_adv(&ui, p);
            return;
        }
        self.set_folder(picked);
    }

    /// New clipboard text: offered in the banner when it is a new link and
    /// the user wants such offers.
    pub(crate) fn clipboard_text(&self, text: &str) {
        let Some(link) = self.filter.borrow_mut().offer(text) else {
            return;
        };
        if !self.settings.borrow().ui.offer_copied_links {
            return;
        }
        let Some(ui) = self.window.upgrade() else {
            return;
        };
        // Hidden in the tray, the offer would wait unseen and be stale by
        // the time the window opens.
        if !ui.window().is_visible() {
            return;
        }
        ui.set_banner(banner_for(&link));
        *self.banner_link.borrow_mut() = Some(link);
    }

    pub(crate) fn banner_download(&self) {
        let link = self.banner_link.borrow_mut().take();
        self.hide_banner();
        if let Some(link) = link {
            self.offer_link(link);
        }
    }

    pub(crate) fn hide_banner(&self) {
        self.banner_link.borrow_mut().take();
        if let Some(ui) = self.window.upgrade() {
            ui.set_banner(BannerVm::default());
        }
    }

    /// The app put `text` on the clipboard: it is not offered back.
    pub fn we_copied(&self, text: &str) {
        self.filter.borrow_mut().we_copied(text);
    }
}

/// Adds `request`; with `sorted`, its type subfolder is made first when
/// missing, inside a folder that exists (a missing drive is reported, not
/// created).
pub(crate) async fn add_job(
    engine: Engine,
    request: AddRequest,
    sorted: bool,
) -> Result<JobId, CoreError> {
    let dest_dir = &request.dest_dir;
    if sorted && dest_dir.parent().is_some_and(Path::is_dir) {
        let _ = tokio::fs::create_dir_all(dest_dir).await;
    }
    engine.add(request).await
}

/// The banner for a link: its host and the kind its file name suggests.
fn banner_for(link: &str) -> BannerVm {
    let (host, kind) = match url::Url::parse(link) {
        Ok(u) if u.scheme() == "magnet" => (String::new(), "magnet"),
        Ok(u) => {
            let name = u
                .path_segments()
                .and_then(|mut s| s.next_back())
                .unwrap_or("");
            let kind = if name.to_ascii_lowercase().ends_with(".torrent") {
                "magnet"
            } else {
                rows::kind_for(name, JobKind::Http)
            };
            (u.host_str().unwrap_or("").to_string(), kind)
        }
        Err(_) => (String::new(), "file"),
    };
    BannerVm {
        visible: true,
        host: host.into(),
        kind: kind.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn video_links_are_shown_by_media_add() {
        let media = taurus_core::media::MediaInfo {
            url: "https://www.youtube.com/watch?v=x".into(),
            title: "t".into(),
            uploader: None,
            duration_secs: None,
            thumbnail_url: None,
            extractor: "Youtube".into(),
            live: taurus_core::media::LiveStatus::NotLive,
            playlist: None,
            qualities: Vec::new(),
            subtitle_languages: Vec::new(),
            auto_caption_languages: Vec::new(),
            all_qualities: Vec::new(),
        };
        assert!(add_vm(&LinkInfo::Media(media)).is_none());
    }

    #[test]
    fn sort_folder_for_kind() {
        assert_eq!(sort_folder("media"), Some("Videos"));
        assert_eq!(sort_folder("live"), Some("Videos"));
        assert_eq!(sort_folder("music"), Some("Music"));
        assert_eq!(sort_folder("image"), Some("Pictures"));
        assert_eq!(sort_folder("file"), Some("Documents"));
        for kind in ["disc", "archive", "magnet"] {
            assert_eq!(sort_folder(kind), None, "{kind}");
        }
    }

    #[test]
    fn destination_rules() {
        let base = Path::new("/data/dl");
        let picked = Path::new("/elsewhere");
        assert_eq!(
            destination(None, Some(base), true, "music"),
            base.join("Music")
        );
        assert_eq!(destination(None, Some(base), true, "archive"), base);
        assert_eq!(destination(None, Some(base), false, "music"), base);
        // A folder picked for this download is used as it is.
        assert_eq!(destination(Some(picked), Some(base), true, "music"), picked);
        assert_eq!(
            destination(None, None, false, "file"),
            default_download_dir()
        );
    }

    #[test]
    fn banner_shows_host_and_kind() {
        let b = banner_for("https://cdn.example.org/a/song.MP3?x=1");
        assert_eq!(
            (b.host.as_str(), b.kind.as_str()),
            ("cdn.example.org", "music")
        );
        assert!(b.visible);
        let b = banner_for("magnet:?xt=urn:btih:abc");
        assert_eq!((b.host.as_str(), b.kind.as_str()), ("", "magnet"));
        let b = banner_for("https://example.org/linux.torrent");
        assert_eq!(b.kind.as_str(), "magnet");
    }
}
