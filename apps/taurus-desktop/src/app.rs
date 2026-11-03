//! The running app: the engine, the window and the glue between them.
//!
//! Everything here runs on the UI thread. Engine calls run on the tokio
//! runtime; what they report back, and what other threads send (forwarded
//! links, clipboard text), goes through one channel that a 100 ms
//! `slint::Timer` drains together with the engine's coalesced events (see
//! `pump.rs`). No `invoke_from_event_loop`: tests drive the same timer with
//! `slint::platform::update_timers_and_animations()`.
//!
//! The add flow lives in `add_flow.rs` (video links in `media_add.rs`,
//! Advanced mode's Add screen in `adv_add.rs`, its Grab links screen in
//! `grab_flow.rs` and Add several links in `batch_flow.rs`),
//! the Settings screen in `settings_flow.rs`, applying engine state to the
//! window in `pump.rs`.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use slint::{CloseRequestResponse, ComponentHandle, SharedPixelBuffer, Timer, TimerMode};
use taurus_core::{
    CoreError, Engine, ErrorKind, GrabResult, JobId, JobState, LinkInfo, RemoveFiles, Settings,
};

use crate::add_flow::PendingAdd;
use crate::adv_add::AdvAddState;
use crate::adv_flow::AdvFlow;
use crate::adv_view::AdvState;
use crate::batch_flow::BatchState;
use crate::bridge::{self, Coalescer};
use crate::grab_flow::GrabState;
use crate::links::ClipboardFilter;
use crate::pump::ViewState;
use crate::settings_flow::SettingsWriter;
use crate::{AppWindow, icon};

/// How often engine state reaches the window, at most.
const REFRESH: Duration = Duration::from_millis(100);
/// How long quitting waits for the engine to save and stop.
const QUIT_TIMEOUT: Duration = Duration::from_secs(10);

/// What closing the main window does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClosePolicy {
    /// Keep running in the tray so downloads go on.
    HideToTray,
    /// No tray to come back from: quit (the engine shuts down cleanly).
    Quit,
}

/// Closing the window hides it when a tray icon exists, otherwise it quits.
pub fn close_policy(has_tray: bool) -> ClosePolicy {
    if has_tray {
        ClosePolicy::HideToTray
    } else {
        ClosePolicy::Quit
    }
}

/// Work for the UI thread, from the runtime or other threads.
pub(crate) enum UiMsg {
    /// `Engine::inspect` answered for the add with this `seq`.
    Inspected {
        seq: u64,
        result: Box<Result<LinkInfo, ErrorKind>>,
    },
    /// `Engine::add` refused the confirmed add.
    AddFailed(ErrorKind),
    /// `Engine::add` answered the unconfirmed add of a large playlist
    /// (the add with this `seq`).
    MediaAdded {
        seq: u64,
        result: Result<(), CoreError>,
    },
    /// Saving the settings failed: show what the engine has.
    SettingsFailed,
    /// Links from a second launch; empty means "show the window".
    Forwarded(Vec<String>),
    /// The clipboard now holds this text.
    Clipboard(String),
    /// Look at this link next (the next one of a forwarded batch).
    Link(String),
    /// `Engine::set_credentials` answered for the sign-in dialog of `id`.
    SignedIn {
        id: i32,
        result: Result<(), ErrorKind>,
    },
    /// `Engine::set_torrent_overrides` refused the rules picked for a job.
    OverridesRefused(JobId),
    /// `Engine::add` answered Download on the Advanced Add screen (the
    /// add with this `seq`).
    AdvAdded {
        seq: u64,
        result: Result<(), CoreError>,
    },
    /// `Engine::grab` answered the grab with this run number.
    Grabbed {
        run: u64,
        result: Box<Result<GrabResult, ErrorKind>>,
    },
}

/// Hands links and clipboard text to the app from any thread.
#[derive(Clone)]
pub struct AppSender(mpsc::Sender<UiMsg>);

impl AppSender {
    /// Links from a second launch (or the command line); empty shows the
    /// window.
    pub fn forwarded(&self, links: Vec<String>) {
        let _ = self.0.send(UiMsg::Forwarded(links));
    }

    /// New clipboard text; offered as a download when it is a link.
    pub fn clipboard(&self, text: String) {
        let _ = self.0.send(UiMsg::Clipboard(text));
    }

    pub(crate) fn send(&self, msg: UiMsg) {
        let _ = self.0.send(msg);
    }
}

pub struct App {
    pub(crate) rt: tokio::runtime::Handle,
    pub(crate) engine: Engine,
    pub(crate) window: slint::Weak<AppWindow>,
    pub(crate) tx: AppSender,
    rx: mpsc::Receiver<UiMsg>,
    has_tray: bool,
    /// Desktop notifications and the login item are used (off in tests).
    pub(crate) os_shell: Cell<bool>,
    quitting: Cell<bool>,
    pub(crate) filter: RefCell<ClipboardFilter>,
    /// The link the clipboard banner offers.
    pub(crate) banner_link: RefCell<Option<String>>,
    /// The link on the Add screen, while it is open.
    pub(crate) pending: RefCell<Option<PendingAdd>>,
    /// Forwarded links waiting for the Add screen.
    pub(crate) queue: RefCell<VecDeque<String>>,
    pub(crate) seq: Cell<u64>,
    /// The settings as the UI last set or received them.
    pub(crate) settings: RefCell<Settings>,
    pub(crate) view: RefCell<ViewState>,
    /// Advanced mode's list (`adv_view.rs`).
    pub(crate) adv: RefCell<AdvState>,
    /// Advanced mode's detail and dialogs (`adv_flow.rs`).
    pub(crate) flow: AdvFlow,
    /// Advanced mode's Add screen and file picker (`adv_add.rs`).
    pub(crate) adv_add: AdvAddState,
    /// Advanced mode's Grab links (`grab_flow.rs`).
    pub(crate) grab: GrabState,
    /// Advanced mode's Add several links (`batch_flow.rs`).
    pub(crate) batch: BatchState,
    coalescer: Arc<Mutex<Coalescer>>,
    pub(crate) writer: SettingsWriter,
    /// Mirrors `settings.ui.offer_copied_links` for the clipboard thread.
    pub(crate) offer_links: Arc<AtomicBool>,
    bridge: RefCell<Option<tokio::task::JoinHandle<()>>>,
    timer: Timer,
}

/// Connects a window callback to an `App` method without keeping the app
/// alive from the window.
macro_rules! on {
    ($ui:expr, $app:expr, $setter:ident, |$a:ident $(, $arg:ident)*| $body:expr) => {{
        let weak = Rc::downgrade($app);
        $ui.$setter(move |$($arg),*| {
            if let Some($a) = weak.upgrade() {
                $body
            }
        });
    }};
}

impl App {
    /// Shows the engine's jobs and settings in `window` and connects every
    /// window callback. The engine must run on `rt`. With `has_tray`,
    /// closing the window hides it; otherwise closing quits.
    pub fn start(
        window: &AppWindow,
        engine: Engine,
        rt: tokio::runtime::Handle,
        has_tray: bool,
    ) -> Rc<App> {
        let (tx, rx) = mpsc::channel();
        let settings = engine.settings();
        let coalescer = Arc::new(Mutex::new(Coalescer::new(REFRESH)));
        let tx = AppSender(tx);
        let writer = SettingsWriter::spawn(&engine, &rt, tx.clone());
        let offer_links = Arc::new(AtomicBool::new(settings.ui.offer_copied_links));
        let app = Rc::new(App {
            rt,
            engine,
            window: window.as_weak(),
            tx,
            rx,
            has_tray,
            os_shell: Cell::new(false),
            quitting: Cell::new(false),
            filter: RefCell::new(ClipboardFilter::new()),
            banner_link: RefCell::new(None),
            pending: RefCell::new(None),
            queue: RefCell::new(VecDeque::new()),
            seq: Cell::new(0),
            settings: RefCell::new(settings),
            view: RefCell::new(ViewState::default()),
            adv: RefCell::new(AdvState::default()),
            flow: AdvFlow::default(),
            adv_add: AdvAddState::default(),
            grab: GrabState::default(),
            batch: BatchState::default(),
            coalescer: coalescer.clone(),
            writer,
            offer_links,
            bridge: RefCell::new(None),
            timer: Timer::default(),
        });

        let size = 64;
        let pixels = SharedPixelBuffer::clone_from_slice(&icon::app_icon_rgba(size), size, size);
        window.set_app_icon(slint::Image::from_rgba8(pixels));
        window.set_has_tray(has_tray);
        window.set_is_macos(cfg!(target_os = "macos"));
        app.view.borrow().install(window);
        app.adv.borrow().install(window);
        app.grab.install(window);
        app.batch.install(window);
        app.push_settings();
        let screen = if app.engine.snapshot().is_empty() {
            crate::Screen::Welcome
        } else {
            crate::Screen::Downloads
        };
        window.set_screen(screen);

        app.start_detail_poll();
        app.connect(window);
        *app.bridge.borrow_mut() = Some(bridge::spawn(&app.engine, &app.rt, coalescer));
        let weak = Rc::downgrade(&app);
        app.timer.start(TimerMode::Repeated, REFRESH, move || {
            if let Some(app) = weak.upgrade() {
                app.tick();
            }
        });
        // The first flush (the seeded jobs) goes out at once.
        app.tick();
        app
    }

    fn connect(self: &Rc<Self>, ui: &AppWindow) {
        on!(ui, self, on_link_entered, |a, text| a.link_entered(&text));
        on!(ui, self, on_link_dropped, |a, text| a.link_entered(&text));
        on!(ui, self, on_add_confirmed, |a| a.add_confirmed());
        on!(ui, self, on_add_cancelled, |a| a.add_cancelled());
        on!(ui, self, on_choose_folder, |a| a.choose_folder());
        on!(ui, self, on_pause, |a, id| a.job_call(id, Job::Pause));
        on!(ui, self, on_resume, |a, id| a.job_call(id, Job::Resume));
        // Ruling R6: resuming a Failed job is the retry.
        on!(ui, self, on_retry, |a, id| a.job_call(id, Job::Resume));
        on!(ui, self, on_stop_recording, |a, id| a
            .job_call(id, Job::StopRecording));
        on!(ui, self, on_media_mode_changed, |a, mode| a
            .media_mode_changed(&mode));
        on!(ui, self, on_quality_cycled, |a| a.quality_cycled());
        on!(ui, self, on_playlist_confirmed, |a| a.playlist_confirmed());
        on!(ui, self, on_remove, |a, id| a.remove(id));
        on!(ui, self, on_open_file, |a, id| a.open_file(id, false));
        on!(ui, self, on_reveal, |a, id| a.open_file(id, true));
        on!(ui, self, on_pause_all, |a| a.pause_all());
        on!(ui, self, on_resume_all, |a| a.resume_all());
        on!(ui, self, on_banner_download, |a| a.banner_download());
        on!(ui, self, on_banner_dismiss, |a| a.hide_banner());
        on!(ui, self, on_setting_changed, |a, key, value| a
            .setting_changed(&key, &value));
        on!(ui, self, on_cancel_countdown, |a| a.cancel_countdown());
        on!(ui, self, on_width_changed, |a| a.apply_layout());
        on!(ui, self, on_select, |a, id| a.adv_select(id));
        on!(ui, self, on_filter_changed, |a, key| a.adv_filter(&key));
        on!(ui, self, on_tab_changed, |a, tab| a.adv_tab(tab));
        on!(ui, self, on_search_changed, |a, text| a.adv_search(&text));
        on!(ui, self, on_copy_link, |a, id| a.copy_link(id));
        on!(ui, self, on_convert, |a, id, preset| a.convert(id, &preset));
        on!(ui, self, on_ask_remove, |a, id| a.ask_remove(id));
        on!(ui, self, on_confirm_remove, |a, id, delete| a
            .confirm_remove(id, delete));
        on!(ui, self, on_retry_with_browser, |a, id, browser| a
            .retry_with_browser(id, &browser));
        on!(ui, self, on_ask_password, |a, id| a.ask_password(id));
        on!(ui, self, on_submit_password, |a, id, user, password| a
            .submit_password(id, user, password));
        on!(ui, self, on_files_chosen, |a, id, indexes| a
            .files_chosen(id, indexes));
        on!(ui, self, on_set_override, |a, id, field, value| a
            .set_override(id, &field, &value));
        on!(ui, self, on_choose_files, |a, id| a.choose_files(id));
        on!(ui, self, on_adv_field_changed, |a, field, value| a
            .adv_field_changed(&field, &value));
        on!(ui, self, on_adv_toggle_file, |a, index| a
            .adv_toggle_file(index));
        on!(ui, self, on_adv_toggle_all, |a, on| a.adv_toggle_all(on));
        on!(ui, self, on_adv_toggle_group, |a, folder| a
            .adv_toggle_group(&folder));
        on!(ui, self, on_adv_add_confirmed, |a, start_now| a
            .adv_add_confirmed(start_now));
        on!(ui, self, on_open_grab, |a, url| a.open_grab(&url));
        on!(ui, self, on_open_batch, |a| a.open_batch());
        let links = ui.global::<crate::LinksUi>();
        on!(links, self, on_grab_page, |a, url| a.grab_page(&url));
        on!(links, self, on_grab_option, |a, key, value| a
            .grab_option(&key, &value));
        on!(links, self, on_grab_toggle, |a, index| a.grab_toggle(index));
        on!(links, self, on_grab_select_shown, |a| a.grab_select_shown());
        on!(links, self, on_grab_download, |a| a.grab_download());
        on!(links, self, on_grab_closed, |a| a.grab_closed());
        on!(links, self, on_batch_edited, |a, text| a
            .batch_edited(&text));
        on!(links, self, on_batch_mirrors, |a, on| a.batch_mirrors(on));
        on!(links, self, on_batch_add, |a| a.batch_add());
        // open-whendone: its menu comes in a later M6b task; until then it
        // is left unconnected.

        let weak = Rc::downgrade(self);
        ui.window().on_close_requested(move || {
            if let Some(app) = weak.upgrade()
                && close_policy(app.has_tray) == ClosePolicy::Quit
            {
                app.quit();
            }
            CloseRequestResponse::HideWindow
        });
    }

    /// Turns on desktop notifications and the login item. Off by default so
    /// tests never touch the OS.
    pub fn use_os_shell(&self) {
        self.os_shell.set(true);
    }

    /// For other threads: forwarded links and clipboard text.
    pub fn sender(&self) -> AppSender {
        self.tx.clone()
    }

    /// True while the user wants copied links offered; the clipboard
    /// watcher reads nothing while it is false.
    pub fn offer_links_flag(&self) -> Arc<AtomicBool> {
        self.offer_links.clone()
    }

    /// Saves every job's progress, stops the engine (waiting at most 10 s)
    /// and ends the event loop. Later calls do nothing.
    pub fn quit(&self) {
        if self.quitting.replace(true) {
            return;
        }
        self.timer.stop();
        if let Some(task) = self.bridge.borrow_mut().take() {
            task.abort();
        }
        // Gone at once, not frozen while the engine saves.
        if let Some(ui) = self.window.upgrade()
            && let Err(e) = ui.hide()
        {
            eprintln!("hyades: cannot hide the window: {e}");
        }
        let engine = self.engine.clone();
        let stopped = self
            .rt
            .block_on(async move { tokio::time::timeout(QUIT_TIMEOUT, engine.shutdown()).await });
        if stopped.is_err() {
            eprintln!("hyades: the engine did not stop within {QUIT_TIMEOUT:?}");
        }
        let _ = slint::quit_event_loop();
    }

    /// Shows the window again (from the tray) and unminimizes it; it is not
    /// raised above other windows.
    pub fn show_window(&self) {
        if let Some(ui) = self.window.upgrade() {
            if let Err(e) = ui.show() {
                eprintln!("hyades: cannot show the window: {e}");
            }
            ui.window().set_minimized(false);
        }
    }

    /// Every unfinished job is paused.
    pub fn all_paused(&self) -> bool {
        self.view.borrow().all_paused()
    }

    pub fn pause_all(&self) {
        for id in
            self.view
                .borrow()
                .ids_in(&[JobState::Downloading, JobState::Queued, JobState::Waiting])
        {
            self.spawn_job(id, Job::Pause);
        }
    }

    pub fn resume_all(&self) {
        for id in self.view.borrow().ids_in(&[JobState::Paused]) {
            self.spawn_job(id, Job::Resume);
        }
    }

    fn job_call(&self, id: i32, job: Job) {
        self.spawn_job(JobId(id.into()), job);
    }

    /// "Remove from list": an unfinished download's partial file goes too.
    fn remove(&self, id: i32) {
        self.job_call(id, Job::Remove);
    }

    fn spawn_job(&self, id: JobId, job: Job) {
        let engine = self.engine.clone();
        self.rt.spawn(async move {
            let result = match job {
                Job::Pause => engine.pause(id).await,
                Job::Resume => engine.resume(id).await,
                // Keeps what was recorded.
                Job::StopRecording => engine.stop_recording(id).await,
                // The engine decides about the files as it removes the job.
                Job::Remove => engine.remove_with(id, RemoveFiles::IfUnfinished).await,
            };
            // A failed resume keeps the job as it was, with its error shown.
            if let Err(e) = result {
                eprintln!("hyades: {job:?} {id:?}: {e}");
            }
        });
    }

    /// Opens a job's file, or shows it in the file manager.
    fn open_file(&self, id: i32, reveal: bool) {
        let Some(path) = self.view.borrow().path_of(JobId(id.into())) else {
            return;
        };
        let result = if !path.exists() {
            opener::open(existing(&path))
        } else if reveal {
            opener::reveal(&path)
        } else {
            opener::open(&path)
        };
        if let Err(e) = result {
            eprintln!("hyades: cannot open {}: {e}", path.display());
        }
    }

    fn cancel_countdown(&self) {
        self.hide_countdown();
        let engine = self.engine.clone();
        self.rt.spawn(async move {
            let _ = engine.cancel_when_done().await;
        });
    }

    /// One refresh: grabbed sizes, messages, then engine state when due.
    fn tick(&self) {
        self.grab_sizes();
        while let Ok(msg) = self.rx.try_recv() {
            self.handle(msg);
            if self.quitting.get() {
                return;
            }
        }
        let flush = bridge::lock(&self.coalescer).take_if_due(std::time::Instant::now());
        match flush {
            Some(flush) => self.apply_flush(flush),
            None => self.refresh_clock(),
        }
    }

    fn handle(&self, msg: UiMsg) {
        match msg {
            UiMsg::Inspected { seq, result } => self.inspected(seq, *result),
            UiMsg::AddFailed(kind) => self.add_failed(kind),
            UiMsg::MediaAdded { seq, result } => self.media_added(seq, result),
            UiMsg::SettingsFailed => self.engine_settings(self.engine.settings()),
            UiMsg::Forwarded(links) => self.forwarded(links),
            UiMsg::Clipboard(text) => self.clipboard_text(&text),
            UiMsg::Link(link) => self.offer_link(link),
            UiMsg::SignedIn { id, result } => self.signed_in(id, result),
            UiMsg::OverridesRefused(id) => self.overrides_refused(id),
            UiMsg::AdvAdded { seq, result } => self.adv_added(seq, result),
            UiMsg::Grabbed { run, result } => self.grabbed(run, *result),
        }
    }

    pub(crate) fn hide_countdown(&self) {
        if let Some(ui) = self.window.upgrade() {
            ui.set_countdown(Default::default());
        }
    }
}

impl Drop for App {
    fn drop(&mut self) {
        if let Some(task) = self.bridge.get_mut().take() {
            task.abort();
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum Job {
    Pause,
    Resume,
    StopRecording,
    Remove,
}

/// `path` if it exists, else its nearest existing parent (a removed file
/// still shows its folder).
fn existing(path: &std::path::Path) -> PathBuf {
    path.ancestors()
        .find(|p| p.exists())
        .unwrap_or(path)
        .to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn close_policy() {
        assert_eq!(super::close_policy(true), ClosePolicy::HideToTray);
        assert_eq!(super::close_policy(false), ClosePolicy::Quit);
    }

    #[test]
    fn existing_walks_up() {
        let dir = std::env::temp_dir();
        assert_eq!(existing(&dir.join("no-such-file-hyades")), dir);
    }
}
