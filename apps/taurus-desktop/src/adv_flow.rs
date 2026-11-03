//! Advanced mode's detail and its actions. The selected download's details
//! are fetched with `Engine::job_detail` when it is selected and then
//! every 500 ms while the detail is on screen (a Slint timer of its own,
//! not the coalesced events; stopped when nothing is selected or the
//! layout is Simple). The detail's and the dialogs' callbacks become engine
//! calls on the app's runtime.
//!
//! The sign-in dialog's user name and password live only in the window's
//! `password-user` / `password-text` until submitted: both are emptied as
//! the engine call starts, and no view model or log ever holds them.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use slint::{ComponentHandle, Model, ModelRc, SharedString, Timer, TimerMode, VecModel};
use taurus_core::media::ConvertPreset;
use taurus_core::{
    ConvertSource, Credentials, ErrorKind, JobDetail, JobId, JobView, RemoveFiles, TorrentOverrides,
};
use tokio::sync::mpsc;

use crate::app::{App, UiMsg};
use crate::clipboard;
use crate::detail::{detail_vm, link_of, warn, with_override};
use crate::pump::{set_model, unix_now};
use crate::{
    AppWindow, DetailVm, FileRowVm, PasswordDialogVm, RemoveDialogVm, Screen, StatVm, StepVm,
};

/// How often the details on screen are fetched again.
const POLL: Duration = Duration::from_millis(500);

/// The detail's models (kept across refreshes, so the files list keeps its
/// place) and what the detail last showed.
pub(crate) struct AdvFlow {
    cells: Rc<VecModel<f32>>,
    files: Rc<VecModel<FileRowVm>>,
    steps: Rc<VecModel<StepVm>>,
    stats: Rc<VecModel<StatVm>>,
    /// The job the detail shows (whether or not its details could be
    /// fetched).
    shown: Cell<Option<JobId>>,
    /// The selected job's details, as last fetched.
    last: RefCell<Option<JobDetail>>,
    poll: Timer,
    /// The job whose sign-in dialog opened by itself (once per selection).
    offered: Cell<Option<JobId>>,
    clipboard: RefCell<clipboard::Writer>,
    /// The sharing rules last picked for the selected torrent, which the
    /// next pick builds on (the view may not show them yet).
    wanted: RefCell<Option<(JobId, TorrentOverrides)>>,
    /// Sharing rules go to the engine one after another, in the order
    /// they were picked (a worker task, started on first use).
    overrides_tx: RefCell<Option<mpsc::UnboundedSender<(JobId, TorrentOverrides)>>>,
    /// Bumped when the engine refused sharing rules: the selects show the
    /// engine's values again (`DetailVm.overrides-epoch`).
    overrides_epoch: Cell<i32>,
}

impl Default for AdvFlow {
    fn default() -> Self {
        Self {
            cells: Rc::new(VecModel::default()),
            files: Rc::new(VecModel::default()),
            steps: Rc::new(VecModel::default()),
            stats: Rc::new(VecModel::default()),
            shown: Cell::new(None),
            last: RefCell::new(None),
            poll: Timer::default(),
            offered: Cell::new(None),
            clipboard: RefCell::new(clipboard::Writer::default()),
            wanted: RefCell::new(None),
            overrides_tx: RefCell::new(None),
            overrides_epoch: Cell::new(0),
        }
    }
}

fn job_id(id: i32) -> JobId {
    JobId(id.into())
}

impl App {
    /// Sets up the detail's timer, stopped until a detail is shown.
    pub(crate) fn start_detail_poll(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        self.flow.poll.start(TimerMode::Repeated, POLL, move || {
            if let Some(app) = weak.upgrade() {
                app.poll_detail();
            }
        });
        self.flow.poll.stop();
    }

    /// Shows `selected` (one of `views`) in the detail, or no detail.
    /// A newly selected job's details are fetched at once; then the timer
    /// keeps them fresh.
    pub(crate) fn show_detail(&self, ui: &AppWindow, views: &[JobView], selected: Option<JobId>) {
        let Some(view) = selected.and_then(|id| views.iter().find(|v| v.id == id)) else {
            self.hide_detail(ui);
            return;
        };
        if self.flow.shown.replace(Some(view.id)) != Some(view.id) {
            self.flow.offered.set(None);
            self.flow.wanted.borrow_mut().take();
            *self.flow.last.borrow_mut() = self.fetch(view.id);
        }
        if !self.flow.poll.running() {
            self.flow.poll.restart();
        }
        self.render_detail(ui, view);
    }

    /// No detail: the timer stops.
    pub(crate) fn hide_detail(&self, ui: &AppWindow) {
        self.flow.poll.stop();
        self.flow.shown.set(None);
        self.flow.last.borrow_mut().take();
        if ui.get_detail().visible {
            ui.set_detail(DetailVm::default());
        }
    }

    fn fetch(&self, id: JobId) -> Option<JobDetail> {
        let _rt = self.rt.enter();
        self.engine.job_detail(id)
    }

    /// The timer: fetches the details again while the detail is on screen
    /// (the Advanced list's screen, in a shown window).
    fn poll_detail(&self) {
        let Some(ui) = self.window.upgrade() else {
            return;
        };
        let Some(id) = self.adv.borrow().selected() else {
            return;
        };
        if !on_screen(&ui) {
            return;
        }
        let view = self.view.borrow().view(id).cloned();
        let Some(view) = view else {
            return;
        };
        *self.flow.last.borrow_mut() = self.fetch(id);
        self.render_detail(&ui, &view);
    }

    fn render_detail(&self, ui: &AppWindow, view: &JobView) {
        let last = self.flow.last.borrow();
        let empty;
        let detail = match last.as_ref() {
            Some(d) => d,
            None => {
                empty = JobDetail {
                    id: view.id,
                    segments: Vec::new(),
                    connections: 0,
                    files: Vec::new(),
                    media_files: Vec::new(),
                    mirrors: Vec::new(),
                    referer: None,
                    checksum: None,
                    has_password: false,
                    overrides: None,
                };
                &empty
            }
        };
        let mut vm = detail_vm(view, detail, unix_now());
        let f = &self.flow;
        set_model(&f.cells, vm.cells.iter(), |_| 0);
        set_model(&f.files, vm.files.iter(), |r| r.index);
        set_model(&f.steps, vm.steps.iter(), |_| 0);
        set_model(&f.stats, vm.stats.iter(), |_| 0);
        vm.cells = ModelRc::from(f.cells.clone());
        vm.files = ModelRc::from(f.files.clone());
        vm.steps = ModelRc::from(f.steps.clone());
        vm.stats = ModelRc::from(f.stats.clone());
        vm.overrides_epoch = f.overrides_epoch.get();
        // The engine shows the sharing rules last picked: nothing pending.
        let caught_up = matches!(
            (&*f.wanted.borrow(), &view.torrent),
            (Some((id, o)), Some(t)) if *id == view.id && *o == t.overrides
        );
        if caught_up {
            f.wanted.borrow_mut().take();
        }
        let needs_password = vm.needs_password;
        if ui.get_detail() != vm {
            ui.set_detail(vm);
        }
        // A download that wants a password asks for it once, when its
        // detail is on screen.
        if needs_password
            && on_screen(ui)
            && self.flow.offered.get() != Some(view.id)
            && !ui.get_password_dialog().visible
        {
            self.flow.offered.set(Some(view.id));
            self.open_password(ui, view, false);
        }
    }

    fn job_view(&self, id: i32) -> Option<JobView> {
        self.view.borrow().view(job_id(id)).cloned()
    }

    /// "Copy link": the link as it was added, without any user name or
    /// password in it (a conversion has no link). The clipboard watcher is
    /// told first, so the link is not offered back as a download.
    pub(crate) fn copy_link(&self, id: i32) {
        let Some(view) = self.job_view(id) else {
            return;
        };
        let link = link_of(&view);
        if link.is_empty() {
            return;
        }
        self.we_copied(&link);
        if let Err(e) = self.flow.clipboard.borrow_mut().set(&link) {
            eprintln!("hyades: cannot copy the link: {e}");
        }
    }

    pub(crate) fn convert(&self, id: i32, preset: &str) {
        let preset = match preset {
            "mp3" => ConvertPreset::Mp3,
            "smaller-mp4" => ConvertPreset::SmallerMp4,
            _ => return,
        };
        let engine = self.engine.clone();
        self.rt.spawn(async move {
            if let Err(e) = engine.convert(ConvertSource::Job(job_id(id)), preset).await {
                warn(format_args!("convert {id}: {e}"));
            }
        });
    }

    pub(crate) fn ask_remove(&self, id: i32) {
        let (Some(view), Some(ui)) = (self.job_view(id), self.window.upgrade()) else {
            return;
        };
        ui.set_remove_dialog(RemoveDialogVm {
            visible: true,
            id,
            name: view.file_name.as_str().into(),
        });
    }

    /// The Remove dialog's answer (ruling R10): finished files stay unless
    /// `delete_files`; an unfinished download's partial files always go
    /// (nothing could clean them up later).
    pub(crate) fn confirm_remove(&self, id: i32, delete_files: bool) {
        if let Some(ui) = self.window.upgrade() {
            ui.set_remove_dialog(RemoveDialogVm::default());
        }
        let files = if delete_files {
            RemoveFiles::All
        } else {
            RemoveFiles::IfUnfinished
        };
        let engine = self.engine.clone();
        self.rt.spawn(async move {
            if let Err(e) = engine.remove_with(job_id(id), files).await {
                warn(format_args!("remove {id}: {e}"));
            }
        });
    }

    pub(crate) fn retry_with_browser(&self, id: i32, browser: &str) {
        let (engine, browser) = (self.engine.clone(), browser.to_owned());
        self.rt.spawn(async move {
            if let Err(e) = engine.retry_with_cookies(job_id(id), browser).await {
                warn(format_args!("sign in with the browser {id}: {e}"));
            }
        });
    }

    /// "Enter password…".
    pub(crate) fn ask_password(&self, id: i32) {
        if let (Some(view), Some(ui)) = (self.job_view(id), self.window.upgrade()) {
            self.open_password(&ui, &view, false);
        }
    }

    fn open_password(&self, ui: &AppWindow, view: &JobView, wrong: bool) {
        ui.set_password_dialog(PasswordDialogVm {
            visible: true,
            id: view.id.0.clamp(0, i32::MAX as i64) as i32,
            name: view.file_name.as_str().into(),
            busy: false,
            wrong,
        });
    }

    /// Hands the typed user name and password to the engine; the window's
    /// copies are emptied first. The dialog waits for the answer.
    pub(crate) fn submit_password(&self, id: i32, user: SharedString, password: SharedString) {
        let Some(ui) = self.window.upgrade() else {
            return;
        };
        ui.set_password_user(SharedString::new());
        ui.set_password_text(SharedString::new());
        let mut dialog = ui.get_password_dialog();
        dialog.busy = true;
        dialog.wrong = false;
        ui.set_password_dialog(dialog);
        let creds = Credentials {
            username: user.to_string(),
            password: password.to_string(),
        };
        drop((user, password));
        let (engine, tx) = (self.engine.clone(), self.tx.clone());
        self.rt.spawn(async move {
            let result = engine.set_credentials(job_id(id), creds).await;
            if let Err(e) = &result {
                warn(format_args!("sign in {id}: {e}"));
            }
            tx.send(UiMsg::SignedIn {
                id,
                result: result.map_err(|e| e.kind),
            });
        });
    }

    /// The engine's answer to `submit_password`: a refused password asks
    /// again; anything else closes the dialog (unless it was cancelled
    /// meanwhile, or now shows another download).
    pub(crate) fn signed_in(&self, id: i32, result: Result<(), ErrorKind>) {
        let Some(ui) = self.window.upgrade() else {
            return;
        };
        let dialog = ui.get_password_dialog();
        if !dialog.visible || dialog.id != id {
            return;
        }
        match result {
            Err(ErrorKind::NeedsSignIn) => ui.set_password_dialog(PasswordDialogVm {
                busy: false,
                wrong: true,
                ..dialog
            }),
            _ => ui.set_password_dialog(PasswordDialogVm::default()),
        }
    }

    /// The torrent file picker's choice (indices into the torrent's files).
    pub(crate) fn files_chosen(&self, id: i32, indexes: ModelRc<i32>) {
        let only: Vec<usize> = indexes
            .iter()
            .filter_map(|i| usize::try_from(i).ok())
            .collect();
        let engine = self.engine.clone();
        self.rt.spawn(async move {
            if let Err(e) = engine.set_torrent_files(job_id(id), only).await {
                warn(format_args!("choose files {id}: {e}"));
            }
        });
    }

    /// A torrent's own sharing rule; the other one is kept (as last picked,
    /// else as the view has it).
    pub(crate) fn set_override(&self, id: i32, field: &str, value: &str) {
        let Some(view) = self.job_view(id) else {
            return;
        };
        let wanted = self.flow.wanted.borrow().clone();
        let current = match wanted {
            Some((job, o)) if job == view.id => o,
            _ => view.torrent.map(|t| t.overrides).unwrap_or_default(),
        };
        let Some(overrides) = with_override(&current, field, value) else {
            return;
        };
        *self.flow.wanted.borrow_mut() = Some((view.id, overrides.clone()));
        let mut tx = self.flow.overrides_tx.borrow_mut();
        let tx = tx.get_or_insert_with(|| {
            let (tx, mut rx) = mpsc::unbounded_channel::<(JobId, TorrentOverrides)>();
            let (engine, ui_tx) = (self.engine.clone(), self.tx.clone());
            self.rt.spawn(async move {
                while let Some((id, overrides)) = rx.recv().await {
                    if let Err(e) = engine.set_torrent_overrides(id, overrides).await {
                        warn(format_args!("sharing rules {}: {e}", id.0));
                        ui_tx.send(UiMsg::OverridesRefused(id));
                    }
                }
            });
            tx
        });
        let _ = tx.send((view.id, overrides));
    }

    /// The engine refused sharing rules for `id`: the next pick builds on
    /// the engine's values again, and the selects show them.
    pub(crate) fn overrides_refused(&self, id: JobId) {
        if self.flow.shown.get() != Some(id) {
            return;
        }
        self.flow.wanted.borrow_mut().take();
        let epoch = &self.flow.overrides_epoch;
        epoch.set(epoch.get().wrapping_add(1));
        self.refresh_adv();
    }
}

/// The Advanced list's screen is on view, in a shown window.
fn on_screen(ui: &AppWindow) -> bool {
    ui.window().is_visible() && matches!(ui.get_screen(), Screen::Downloads | Screen::Welcome)
}
