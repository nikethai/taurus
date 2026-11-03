//! Advanced mode's Add screen and the torrent file picker. A link goes
//! through the same steps as on the Simple Add screen (`add_flow.rs`: the
//! pending add, `Engine::inspect`, the destination folder, the forwarded
//! queue); what it turned out to be also becomes an `AdvPick`
//! (`adv_pick.rs`), shown in `adv-add` while the layout is Advanced.
//!
//! Download stays on the screen until the engine answers: a refusal is
//! shown there with everything picked kept but the password. The password
//! typed for a file link lives only in the window's `adv-password` until
//! Download, which empties it as it moves the text into the request (and
//! the window empties it when the screen goes).
//!
//! "Choose files…" of a torrent in the list opens the same picker over the
//! window (`files-dialog`), prefilled from `Engine::job_detail`; while it
//! is open the picker callbacks change it, not the Add screen's.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use slint::{Model, ModelRc, SharedString, VecModel};
use taurus_core::torrent::source::{Source, classify};
use taurus_core::{CoreError, ErrorKind, JobId, LinkInfo};

use crate::add_flow::{PendingAdd, add_job};
use crate::adv_pick::AdvPick;
use crate::app::{App, UiMsg};
use crate::detail::warn;
use crate::pump::set_model;
use crate::torrent_pick::Picker;
use crate::{
    AddVm, AdvAddVm, AppWindow, FilesDialogVm, LangVm, QualityVm, Screen, TorrentFileVm, format,
};

/// A torrent in the list whose files are being chosen.
struct FilesPick {
    id: i32,
    name: String,
    dest: PathBuf,
    picker: Picker,
}

/// The screen's lists (kept, so long lists keep their place) and the
/// files dialog's picker.
#[derive(Default)]
pub(crate) struct AdvAddState {
    qualities: Rc<VecModel<QualityVm>>,
    langs: Rc<VecModel<LangVm>>,
    files: Rc<VecModel<TorrentFileVm>>,
    dialog_files: Rc<VecModel<TorrentFileVm>>,
    dialog: RefCell<Option<FilesPick>>,
}

/// The free space of the disk holding `dir` (or its nearest existing
/// folder: a type subfolder is made only when adding).
fn free_space(dir: &Path) -> Option<u64> {
    let existing = dir.ancestors().find(|p| p.is_dir())?;
    taurus_core::system::available_space(existing).ok()
}

fn is_web_link(url: &str) -> bool {
    matches!(classify(url), Ok(Source::Http(_)))
}

impl App {
    /// A new link is being looked at.
    pub(crate) fn adv_loading(&self, ui: &AppWindow, folder: SharedString) {
        ui.set_adv_password(SharedString::new());
        ui.set_adv_add(AdvAddVm {
            loading: true,
            checksum_ok: true,
            folder,
            ..Default::default()
        });
    }

    /// The link is already in the list (`add` as the Simple screen shows it).
    pub(crate) fn adv_duplicate(&self, ui: &AppWindow, add: &AddVm) {
        let kind = if add.is_media {
            "media"
        } else if add.is_torrent {
            "torrent"
        } else {
            "file"
        };
        ui.set_adv_password(SharedString::new());
        ui.set_adv_add(AdvAddVm {
            duplicate: true,
            kind: kind.into(),
            badge: add.kind.clone(),
            name: add.name.clone(),
            title: add.name.clone(),
            size_text: add.size_text.clone(),
            folder: add.folder.clone(),
            thumbnail: add.thumbnail.clone(),
            checksum_ok: true,
            ..Default::default()
        });
    }

    /// `Engine::inspect` answered for `p`. A web link that wants a user
    /// name and password can still be added, with them.
    pub(crate) fn adv_inspected(
        &self,
        ui: &AppWindow,
        p: &mut PendingAdd,
        result: &Result<LinkInfo, ErrorKind>,
    ) {
        p.adv = match result {
            Ok(info) => Some(
                AdvPick::of(info)
                    .with_cookies(&p.adv_cookies)
                    .with_connections(self.settings.borrow().connections),
            ),
            Err(ErrorKind::NeedsSignIn) if is_web_link(&p.url) => Some(
                AdvPick::signed_in_file(&p.url)
                    .with_connections(self.settings.borrow().connections),
            ),
            Err(kind) => {
                self.adv_error(ui, *kind);
                return;
            }
        };
        if let Some(pick) = &p.adv {
            p.kind = Some(pick.folder_kind());
        }
        self.render_adv(ui, p);
    }

    /// The link cannot be added: why.
    pub(crate) fn adv_error(&self, ui: &AppWindow, kind: ErrorKind) {
        ui.set_adv_add(AdvAddVm {
            error_kind: kind.as_str().into(),
            checksum_ok: true,
            ..Default::default()
        });
    }

    /// Shows `p` as picked now: its lists in the kept models, the real
    /// destination and its free space.
    pub(crate) fn render_adv(&self, ui: &AppWindow, p: &PendingAdd) {
        let Some(pick) = &p.adv else {
            return;
        };
        let kind = pick.folder_kind();
        let mut vm = pick.vm();
        let s = &self.adv_add;
        set_model(&s.qualities, vm.qualities.iter(), |_| 0);
        set_model(&s.langs, vm.subtitle_langs.iter(), |_| 0);
        set_model(&s.files, vm.files.iter(), |r| r.index);
        vm.qualities = ModelRc::from(s.qualities.clone());
        vm.subtitle_langs = ModelRc::from(s.langs.clone());
        vm.files = ModelRc::from(s.files.clone());
        vm.folder = self.destination_text(p, kind);
        let free = free_space(&self.destination_of(p, kind).0);
        vm.free_text = free.map(format::bytes).unwrap_or_default().into();
        vm.short_of_space = free.is_some_and(|f| pick.needed() > f);
        ui.set_adv_add(vm);
    }

    /// A choice on the screen; "sign-in-browser" reads the link again,
    /// signed in with that browser's cookies.
    pub(crate) fn adv_field_changed(&self, field: &str, value: &str) {
        let Some(ui) = self.window.upgrade() else {
            return;
        };
        if field == "sign-in-browser" {
            return self.adv_sign_in(&ui, value);
        }
        let mut pending = self.pending.borrow_mut();
        let Some(p) = pending.as_mut() else {
            return;
        };
        let Some(pick) = p.adv.as_mut().filter(|a| !a.busy) else {
            return;
        };
        if pick.set(field, value) {
            pick.refused = None;
            p.kind = Some(pick.folder_kind());
            self.render_adv(&ui, p);
        }
    }

    /// Looks at the pending link again with `browser`'s cookies (a video
    /// that needs a sign-in, or a bot check).
    fn adv_sign_in(&self, ui: &AppWindow, browser: &str) {
        if taurus_core::media::validate_browser(browser).is_err() {
            return;
        }
        let mut pending = self.pending.borrow_mut();
        let Some(p) = pending.as_mut().filter(|p| !p.url.is_empty()) else {
            return;
        };
        if p.adv.as_ref().is_some_and(|a| a.busy) {
            return;
        }
        let seq = self.seq.get() + 1;
        self.seq.set(seq);
        p.seq = seq;
        p.adv = None;
        p.kind = None;
        p.media = None;
        p.adv_cookies = browser.to_owned();
        let folder = ui.get_adv_add().folder;
        self.adv_loading(ui, folder);
        ui.set_add(AddVm {
            loading: true,
            ..Default::default()
        });
        let (engine, tx) = (self.engine.clone(), self.tx.clone());
        let (url, browser) = (p.url.clone(), browser.to_owned());
        self.rt.spawn(async move {
            let result = engine
                .inspect_with_cookies(&url, Some(&browser))
                .await
                .map_err(|e| e.kind);
            tx.send(UiMsg::Inspected {
                seq,
                result: Box::new(result),
            });
        });
    }

    /// A picker change: the files dialog's while it is open, else the Add
    /// screen's.
    fn adv_pick_files(&self, change: impl FnOnce(&mut Picker)) {
        let Some(ui) = self.window.upgrade() else {
            return;
        };
        if ui.get_files_dialog().visible {
            if let Some(d) = self.adv_add.dialog.borrow_mut().as_mut() {
                change(&mut d.picker);
            }
            return self.render_files_dialog(&ui);
        }
        let mut pending = self.pending.borrow_mut();
        let Some(p) = pending.as_mut() else {
            return;
        };
        let Some(pick) = p.adv.as_mut().filter(|a| !a.busy && a.kind() == "torrent") else {
            return;
        };
        change(pick.picker_mut());
        pick.refused = None;
        self.render_adv(&ui, p);
    }

    pub(crate) fn adv_toggle_file(&self, index: i32) {
        if let Ok(index) = usize::try_from(index) {
            self.adv_pick_files(|p| p.toggle_file(index));
        }
    }

    pub(crate) fn adv_toggle_all(&self, on: bool) {
        self.adv_pick_files(|p| p.toggle_all(on));
    }

    pub(crate) fn adv_toggle_group(&self, folder: &str) {
        self.adv_pick_files(|p| p.toggle_group(folder));
    }

    /// Download: the request goes to the engine with the password typed
    /// (taken out of the window as the request is built, not before);
    /// then the sharing rule picked, and `start_now` when asked. The
    /// screen waits for the answer. After a refusal the password must be
    /// typed again: an empty one is not sent.
    pub(crate) fn adv_add_confirmed(&self, start_now: bool) {
        let Some(ui) = self.window.upgrade() else {
            return;
        };
        let mut pending = self.pending.borrow_mut();
        let Some(p) = pending.as_mut() else {
            return;
        };
        let Some(kind) = p
            .adv
            .as_ref()
            .filter(|a| a.ready())
            .map(AdvPick::folder_kind)
        else {
            return;
        };
        let (dest_dir, sorted) = self.destination_of(p, kind);
        let Some(pick) = p.adv.as_mut() else {
            return;
        };
        let password = ui.get_adv_password();
        if password.is_empty() && pick.wants_password_again() {
            return;
        }
        ui.set_adv_password(SharedString::new());
        let request = pick.request(&p.url, dest_dir, password.to_string());
        drop(password);
        let overrides = pick.overrides();
        pick.busy = true;
        pick.refused = None;
        pick.password_again = false;
        let seq = p.seq;
        self.render_adv(&ui, p);
        let (engine, tx) = (self.engine.clone(), self.tx.clone());
        self.rt.spawn(async move {
            let added = add_job(engine.clone(), request, sorted).await;
            if let Ok(id) = added {
                if let Some(o) = overrides
                    && let Err(e) = engine.set_torrent_overrides(id, o).await
                {
                    warn(format_args!("sharing rules {}: {e}", id.0));
                }
                if start_now && let Err(e) = engine.start_now(id).await {
                    warn(format_args!("start {}: {e}", id.0));
                }
            }
            tx.send(UiMsg::AdvAdded {
                seq,
                result: added.map(|_| ()),
            });
        });
    }

    /// The engine answered Download: added → the list (and the next
    /// queued link); refused → said on the screen, the choices kept. An
    /// answer for an add the user left is shown like a Simple refusal.
    pub(crate) fn adv_added(&self, seq: u64, result: Result<(), CoreError>) {
        let Some(ui) = self.window.upgrade() else {
            return;
        };
        let mut pending = self.pending.borrow_mut();
        let current = pending
            .as_mut()
            .filter(|p| p.seq == seq && p.adv.as_ref().is_some_and(|a| a.busy));
        match (current, result) {
            (Some(_), Ok(())) => {
                pending.take();
                drop(pending);
                ui.set_screen(Screen::Downloads);
                self.next_queued();
            }
            (Some(p), Err(CoreError { kind, detail })) => {
                warn(format_args!("add failed: {detail}"));
                if let Some(pick) = p.adv.as_mut() {
                    pick.busy = false;
                    pick.refused = Some(kind);
                    // The password went with the request: the field is
                    // empty and says to type it again.
                    pick.password_again = true;
                }
                self.render_adv(&ui, p);
            }
            (None, Ok(())) => {}
            (None, Err(CoreError { kind, detail })) => {
                warn(format_args!("add failed: {detail}"));
                drop(pending);
                self.add_failed(kind);
            }
        }
    }

    /// "Choose files…" of a torrent in the list: the picker over the
    /// window, with the files selected now.
    pub(crate) fn choose_files(&self, id: i32) {
        let Some(ui) = self.window.upgrade() else {
            return;
        };
        let job = JobId(id.into());
        let Some(view) = self.view.borrow().view(job).cloned() else {
            return;
        };
        let files = {
            let _rt = self.rt.enter();
            self.engine.job_detail(job).map(|d| d.files)
        };
        let Some(files) = files.filter(|f| !f.is_empty()) else {
            return;
        };
        *self.adv_add.dialog.borrow_mut() = Some(FilesPick {
            id,
            name: view.file_name,
            dest: view.dest_dir,
            picker: Picker::from_progress(&files),
        });
        self.render_files_dialog(&ui);
    }

    fn render_files_dialog(&self, ui: &AppWindow) {
        let dialog = self.adv_add.dialog.borrow();
        let Some(d) = dialog.as_ref() else {
            return;
        };
        let rows = d.picker.rows();
        set_model(&self.adv_add.dialog_files, rows.into_iter(), |r| r.index);
        let free = free_space(&d.dest);
        let chosen: Vec<i32> = d
            .picker
            .chosen()
            .into_iter()
            .filter_map(|i| i32::try_from(i).ok())
            .collect();
        ui.set_files_dialog(FilesDialogVm {
            visible: true,
            id: d.id,
            name: d.name.as_str().into(),
            files: ModelRc::from(self.adv_add.dialog_files.clone()),
            selected_count: d.picker.selected_count().min(i32::MAX as usize) as i32,
            selected_size: format::bytes(d.picker.selected_size()).into(),
            free_text: free.map(format::bytes).unwrap_or_default().into(),
            short_of_space: free.is_some_and(|f| d.picker.needed() > f),
            chosen: ModelRc::new(VecModel::from(chosen)),
        });
    }
}
