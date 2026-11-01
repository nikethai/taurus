//! UI tests run against Slint's headless testing backend. The backend can be
//! initialized only once per process, so this file holds exactly one `#[test]`;
//! later tasks add named helper functions and call them, in order, from it.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use i_slint_backend_testing::{AccessibleRole, ElementHandle, ElementQuery};
use slint::platform::WindowEvent;
use slint::{ComponentHandle, Model, ModelRc, VecModel};
use taurus_core::torrent::hub::TorrentTestNet;
use taurus_core::{
    AddRequest, Engine, EngineConfig, ErrorKind, Event, JobState, PowerMode, Settings, SpeedLimit,
    ThemeChoice, Timing, WhenDone,
};
use taurus_desktop::app::App;
use taurus_desktop::shell_text::simple_message;
use taurus_desktop::{AppWindow, Messages, RowVm, Screen, Theme};
use taurus_testkit::{FailAfter, FileSpec, TestServer, TorrentSeeder};
use tokio::sync::broadcast::error::TryRecvError;

#[test]
fn ui_smoke() {
    i_slint_backend_testing::init_integration_test_with_system_time();
    let ui = AppWindow::new().unwrap();
    window_title(&ui);
    theme_defaults(&ui);
    welcome_has_paste_field(&ui);
    downloads_rows_show_status_and_retry(&ui);
    shell_text_mirrors_messages(&ui);
    let ctx = Ctx::start(&ui);
    add_flow_shows_file_and_finishes(&ui, &ctx);
    settings_change_is_saved_and_shown(&ui, &ctx);
    quick_setting_changes_end_on_the_last(&ui, &ctx);
    close_hides_to_tray_and_keeps_downloading(&ctx);
    failed_row_explains_and_retries(&ui, &ctx);
    duplicate_link_is_flagged(&ui, &ctx);
    waiting_row_counts_down();
    torrent_row_shows_sharing();
    countdown_cancel();
    quit_shuts_down_engine_cleanly(&ui, &ctx);
}

fn window_title(ui: &AppWindow) {
    assert_eq!(ui.get_window_title(), "Hyades");
}

fn theme_defaults(ui: &AppWindow) {
    let theme = ui.global::<Theme>();
    let color = |r, g, b| slint::Color::from_rgb_u8(r, g, b);
    theme.set_dark(false);
    assert_eq!(theme.get_bg().color(), color(0xFB, 0xFB, 0xFC));
    theme.set_dark(true);
    assert_eq!(theme.get_bg().color(), color(0x1C, 0x1C, 0x1F));
    assert_eq!(theme.get_accent_base().color(), color(0xE8, 0x59, 0x0C));
}

/// Elements whose accessible label is exactly `label`.
fn labelled(ui: &AppWindow, label: &str) -> Vec<ElementHandle> {
    ElementHandle::find_by_accessible_label(ui, label).collect()
}

fn welcome_has_paste_field(ui: &AppWindow) {
    ui.set_screen(Screen::Welcome);
    let field = labelled(ui, "Paste a link");
    assert!(
        field
            .iter()
            .any(|e| e.accessible_role() == Some(AccessibleRole::TextInput)),
        "welcome_has_paste_field: no text input labelled \"Paste a link\""
    );
}

fn row(id: i32, state: &str) -> RowVm {
    RowVm {
        id,
        name: format!("file-{id}.zip").into(),
        kind: "archive".into(),
        state: state.into(),
        progress: 0.3,
        done_text: "120 MB".into(),
        total_text: "400 MB".into(),
        eta_kind: "unknown".into(),
        ..Default::default()
    }
}

fn downloads_rows_show_status_and_retry(ui: &AppWindow) {
    let downloading = RowVm {
        eta_kind: "minutes".into(),
        eta_value: 3,
        ..row(1, "downloading")
    };
    let waiting = RowVm {
        retry_seconds: 65,
        error_kind: "connection_lost".into(),
        ..row(2, "waiting")
    };
    let failed = RowVm {
        error_kind: "link_expired".into(),
        ..row(3, "failed")
    };
    let rows = VecModel::from(vec![downloading, waiting, failed]);
    ui.set_downloading_rows(ModelRc::new(rows));
    ui.set_tab(0);
    ui.set_screen(Screen::Downloads);

    let retried = Rc::new(RefCell::new(Vec::new()));
    let sink = retried.clone();
    ui.on_retry(move |id| sink.borrow_mut().push(id));

    let visible = |label: &str| {
        let found = labelled(ui, label);
        assert!(
            found.iter().any(|e| e.size().width > 0.0),
            "downloads_rows_show_status_and_retry: no visible text {label:?}"
        );
    };
    visible("120 MB of 400 MB · About 3 min left");
    visible("Waiting for a connection. Trying again in 1:05.");
    visible("This link stopped working. It may have expired.");

    let buttons: Vec<ElementHandle> = labelled(ui, "Try again")
        .into_iter()
        .filter(|e| e.accessible_role() == Some(AccessibleRole::Button))
        .collect();
    assert_eq!(
        buttons.len(),
        1,
        "downloads_rows_show_status_and_retry: expected one \"Try again\" button"
    );
    buttons[0].invoke_accessible_default_action();
    assert_eq!(
        *retried.borrow(),
        vec![3],
        "downloads_rows_show_status_and_retry: retry(id) not called once with the failed row"
    );
}

/// The OS notification text (Rust) says exactly what the screens say.
fn shell_text_mirrors_messages(ui: &AppWindow) {
    let messages = ui.global::<Messages>();
    for kind in ErrorKind::ALL {
        assert_eq!(
            simple_message(kind),
            messages.invoke_simple(kind.as_str().into()).as_str(),
            "shell_text_mirrors_messages: {kind:?}"
        );
    }
}

/// A real engine in a temp dir, a local web server and the app on top.
struct Ctx {
    rt: tokio::runtime::Runtime,
    server: TestServer,
    tmp: tempfile::TempDir,
    app: Rc<App>,
    engine: Engine,
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
}

impl Ctx {
    fn start(ui: &AppWindow) -> Ctx {
        Ctx::start_with(ui, runtime(), |_, _| {})
    }

    /// Like `start`, on `rt`, with the engine's config and first settings
    /// adjusted by `configure`.
    fn start_with(
        ui: &AppWindow,
        rt: tokio::runtime::Runtime,
        configure: impl FnOnce(&mut EngineConfig, &mut Settings),
    ) -> Ctx {
        let tmp = tempfile::tempdir().unwrap();
        let server = rt.block_on(TestServer::start());
        let mut config = EngineConfig::new(tmp.path().join("data"));
        let out = tmp.path().join("out");
        std::fs::create_dir_all(&out).unwrap();
        let mut settings = Settings::default();
        settings.ui.folder = Some(out);
        configure(&mut config, &mut settings);
        let engine = rt.block_on(Engine::open(config)).unwrap();
        rt.block_on(engine.set_settings(settings)).unwrap();
        let app = App::start(ui, engine.clone(), rt.handle().clone(), false);
        Ctx {
            rt,
            server,
            tmp,
            app,
            engine,
        }
    }

    /// Stops the app and the engine (for a `Ctx` of one check).
    fn shutdown(self, what: &str) {
        drop(self.app);
        self.rt
            .block_on(self.engine.shutdown())
            .unwrap_or_else(|e| panic!("{what}: engine shutdown: {e}"));
    }

    fn out(&self) -> PathBuf {
        self.tmp.path().join("out")
    }

    fn data_dir(&self) -> PathBuf {
        self.tmp.path().join("data")
    }

    /// Where a plain file goes: the "Documents" sort folder of the folder
    /// set in the settings.
    fn documents(&self) -> PathBuf {
        self.tmp.path().join("out").join("Documents")
    }
}

/// Drives Slint timers until `done` holds; panics with `what` after 30 s.
fn wait_until(what: &str, done: impl FnMut() -> bool) {
    wait_for(Duration::from_secs(30), what, done);
}

/// Drives Slint timers until `done` holds; panics with `what` after `limit`.
fn wait_for(limit: Duration, what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + limit;
    loop {
        slint::platform::update_timers_and_animations();
        if done() {
            return;
        }
        assert!(Instant::now() < deadline, "{what}: timed out");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Pastes `url`, waits for the Add screen to show `name`, confirms.
fn add_link(ui: &AppWindow, url: &str, name: &str, what: &str) {
    ui.invoke_link_entered(url.into());
    assert_eq!(
        ui.get_screen(),
        Screen::Add,
        "{what}: not on the Add screen"
    );
    wait_until(&format!("{what}: Add screen shows {name}"), || {
        let add = ui.get_add();
        !add.loading && add.name == name
    });
    ui.invoke_add_confirmed();
    assert_eq!(
        ui.get_screen(),
        Screen::Downloads,
        "{what}: not on the Downloads screen after confirming"
    );
}

fn add_flow_shows_file_and_finishes(ui: &AppWindow, ctx: &Ctx) {
    const WHAT: &str = "add_flow_shows_file_and_finishes";
    let url = ctx.server.serve("x.bin", FileSpec::new(2 * 1024 * 1024));
    ui.invoke_link_entered(url.as_str().into());
    assert_eq!(
        ui.get_screen(),
        Screen::Add,
        "{WHAT}: not on the Add screen"
    );
    wait_until(&format!("{WHAT}: Add screen shows x.bin, 2.1 MB"), || {
        let add = ui.get_add();
        !add.loading && add.name == "x.bin" && add.size_text == "2.1 MB"
    });
    let add = ui.get_add();
    assert_eq!(
        (add.error_kind.as_str(), add.duplicate, add.is_torrent),
        ("", false, false),
        "{WHAT}: {add:?}"
    );
    // "Save to" names the real folder, type subfolder included.
    assert_eq!(
        PathBuf::from(add.folder.as_str()),
        ctx.documents(),
        "{WHAT}: Save to"
    );
    ui.invoke_add_confirmed();
    assert_eq!(
        ui.get_screen(),
        Screen::Downloads,
        "{WHAT}: not on Downloads"
    );
    wait_until(&format!("{WHAT}: one finished row"), || {
        ui.get_finished_rows().row_count() == 1
    });
    let row = ui.get_finished_rows().row_data(0).unwrap();
    assert_eq!((row.name.as_str(), row.state.as_str()), ("x.bin", "done"));
    assert_eq!(ui.get_summary().finished, 1, "{WHAT}: summary");
    let file = ctx.documents().join("x.bin");
    assert_eq!(
        std::fs::metadata(&file).map(|m| m.len()).ok(),
        Some(2 * 1024 * 1024),
        "{WHAT}: {} not saved in full",
        file.display()
    );
}

/// A Settings control's change is saved in the engine and shown back,
/// theme included.
fn settings_change_is_saved_and_shown(ui: &AppWindow, ctx: &Ctx) {
    const WHAT: &str = "settings_change_is_saved_and_shown";
    ui.invoke_setting_changed("theme".into(), "dark".into());
    ui.invoke_setting_changed("accent".into(), "jade".into());
    let shown = ui.get_settings();
    assert_eq!(
        (shown.theme.as_str(), shown.accent.as_str()),
        ("dark", "jade"),
        "{WHAT}: not echoed"
    );
    let theme = ui.global::<Theme>();
    assert_eq!(
        (theme.get_scheme().as_str(), theme.get_accent().as_str()),
        ("dark", "jade"),
        "{WHAT}: theme not applied"
    );
    wait_until(&format!("{WHAT}: saved in the engine"), || {
        let ui_settings = ctx.engine.settings().ui;
        ui_settings.theme == ThemeChoice::Dark && ui_settings.accent == "Jade"
    });
    // An unknown value changes nothing.
    ui.invoke_setting_changed("accent".into(), "mauve".into());
    assert_eq!(
        ui.get_settings().accent.as_str(),
        "jade",
        "{WHAT}: bad value"
    );
}

/// Bursts of changes are saved in order: the engine and the screen end on
/// the last one, also after the engine's echoes of the earlier ones.
fn quick_setting_changes_end_on_the_last(ui: &AppWindow, ctx: &Ctx) {
    const WHAT: &str = "quick_setting_changes_end_on_the_last";
    for i in 0..30 {
        let speed = if i % 2 == 0 { "gentle" } else { "fastest" };
        ui.invoke_setting_changed("speed".into(), speed.into());
        if i % 7 == 0 {
            // Let some saves and echoes through mid-burst.
            slint::platform::update_timers_and_animations();
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    ui.invoke_setting_changed("speed".into(), "balanced".into());
    assert_eq!(ui.get_settings().speed.as_str(), "balanced", "{WHAT}: echo");
    wait_until(&format!("{WHAT}: saved"), || {
        ctx.engine.settings().speed == SpeedLimit::Balanced
    });
    // Several flushes later (every engine echo has arrived) nothing moved.
    let settle = Instant::now() + Duration::from_millis(500);
    while Instant::now() < settle {
        slint::platform::update_timers_and_animations();
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        ui.get_settings().speed.as_str(),
        "balanced",
        "{WHAT}: shown"
    );
    assert_eq!(
        ctx.engine.settings().speed,
        SpeedLimit::Balanced,
        "{WHAT}: stored"
    );
    ui.invoke_setting_changed("speed".into(), "fastest".into());
    wait_until(&format!("{WHAT}: reset"), || {
        ctx.engine.settings().speed == SpeedLimit::Fastest
    });
}

/// With a tray, closing the window only hides it: downloads go on and the
/// engine keeps running.
fn close_hides_to_tray_and_keeps_downloading(ctx: &Ctx) {
    const WHAT: &str = "close_hides_to_tray_and_keeps_downloading";
    let ui = AppWindow::new().unwrap();
    let app = App::start(&ui, ctx.engine.clone(), ctx.rt.handle().clone(), true);
    assert!(ui.get_has_tray(), "{WHAT}: has-tray not set");
    let mut spec = FileSpec::new(20 * 1024 * 1024);
    spec.bytes_per_sec = Some(256 * 1024);
    let url = ctx.server.serve("tray.bin", spec);
    let request = AddRequest {
        url,
        dest_dir: ctx.tmp.path().join("out"),
        ..Default::default()
    };
    let id = ctx.rt.block_on(ctx.engine.add(request)).unwrap();
    let state = || {
        ctx.engine
            .snapshot()
            .into_iter()
            .find(|v| v.id == id)
            .map(|v| v.state)
    };
    wait_until(&format!("{WHAT}: downloading"), || {
        state() == Some(JobState::Downloading)
    });
    ui.show().unwrap();
    ui.window().dispatch_event(WindowEvent::CloseRequested);
    assert!(!ui.window().is_visible(), "{WHAT}: window still shown");
    // The pump keeps running and the engine keeps downloading.
    let settle = Instant::now() + Duration::from_millis(500);
    while Instant::now() < settle {
        slint::platform::update_timers_and_animations();
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(state(), Some(JobState::Downloading), "{WHAT}: job stopped");
    let rows = ui.get_downloading_rows();
    assert!(
        rows.iter()
            .any(|r| r.name == "tray.bin" && r.state == "downloading"),
        "{WHAT}: hidden window no longer updated"
    );
    drop(app);
    ctx.rt
        .block_on(ctx.engine.remove(id, true))
        .unwrap_or_else(|e| panic!("{WHAT}: engine stopped by the close: {e}"));
}

fn quit_shuts_down_engine_cleanly(ui: &AppWindow, ctx: &Ctx) {
    const WHAT: &str = "quit_shuts_down_engine_cleanly";
    let mut spec = FileSpec::new(20 * 1024 * 1024);
    spec.bytes_per_sec = Some(256 * 1024);
    let url = ctx.server.serve("slow.bin", spec);
    add_link(ui, &url, "slow.bin", WHAT);
    wait_until(&format!("{WHAT}: slow.bin downloading"), || {
        let rows = ui.get_downloading_rows();
        rows.iter()
            .any(|r| r.name == "slow.bin" && r.state == "downloading" && r.progress > 0.0)
    });

    ctx.app.quit();

    let stored = Engine::read_snapshot(&EngineConfig::new(ctx.data_dir())).unwrap();
    let job = stored
        .iter()
        .find(|v| v.file_name == "slow.bin")
        .unwrap_or_else(|| panic!("{WHAT}: slow.bin not stored: {stored:?}"));
    assert!(
        matches!(job.state, JobState::Queued | JobState::Paused),
        "{WHAT}: stored as {:?}",
        job.state
    );
    assert!(job.downloaded > 0, "{WHAT}: no progress stored: {job:?}");
    assert!(
        ctx.documents().join("slow.bin.tapa").is_file(),
        "{WHAT}: partial file missing"
    );
    // The quit released the data dir: another engine can open it.
    ctx.rt.block_on(async {
        let engine = Engine::open(EngineConfig::new(ctx.data_dir()))
            .await
            .unwrap_or_else(|e| panic!("{WHAT}: data dir still held after quit: {e}"));
        engine.shutdown().await.unwrap();
    });
}

/// Texts of the elements whose accessible role is text and whose label
/// contains `needle`.
fn texts_containing(ui: &AppWindow, needle: &str) -> Vec<String> {
    ElementQuery::from_root(ui)
        .match_descendants()
        .match_accessible_role(AccessibleRole::Text)
        .find_all()
        .into_iter()
        .filter_map(|e| e.accessible_label())
        .filter(|label| label.contains(needle))
        .map(|label| label.to_string())
        .collect()
}

/// The one button labelled `label`.
fn only_button(ui: &AppWindow, label: &str, what: &str) -> ElementHandle {
    let mut buttons: Vec<ElementHandle> = labelled(ui, label)
        .into_iter()
        .filter(|e| e.accessible_role() == Some(AccessibleRole::Button))
        .collect();
    assert_eq!(
        buttons.len(),
        1,
        "{what}: expected one {label:?} button, found {}",
        buttons.len()
    );
    buttons.remove(0)
}

/// Drives Slint timers for `time` (to see that nothing changes).
fn settle(time: Duration) {
    let end = Instant::now() + time;
    while Instant::now() < end {
        slint::platform::update_timers_and_animations();
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// A download whose link stops working shows why in plain words, and its
/// "Try again" button (pressed through the UI) downloads it once the link
/// works again.
fn failed_row_explains_and_retries(ui: &AppWindow, ctx: &Ctx) {
    const WHAT: &str = "failed_row_explains_and_retries";
    const SIZE: usize = 1024 * 1024;
    let mut spec = FileSpec::new(SIZE);
    // The probe at add succeeds; the download then gets 404.
    spec.fail_after = Some(FailAfter {
        after_requests: 1,
        status: 404,
        headers: vec![],
    });
    let url = ctx.server.serve("expired.bin", spec);
    ui.set_tab(0);
    ui.set_screen(Screen::Downloads);
    let request = AddRequest {
        url,
        dest_dir: ctx.out(),
        ..Default::default()
    };
    ctx.rt.block_on(ctx.engine.add(request)).unwrap();
    wait_until(&format!("{WHAT}: expired.bin failed"), || {
        ui.get_downloading_rows()
            .iter()
            .any(|r| r.name == "expired.bin" && r.state == "failed")
    });
    let row = ui
        .get_downloading_rows()
        .iter()
        .find(|r| r.name == "expired.bin")
        .unwrap();
    assert_eq!(row.error_kind.as_str(), "link_expired", "{WHAT}: {row:?}");
    let reason = labelled(ui, "This link stopped working. It may have expired.");
    assert!(
        reason.iter().any(|e| e.size().width > 0.0),
        "{WHAT}: the row does not say why"
    );

    ctx.server.replace("expired.bin", FileSpec::new(SIZE));
    only_button(ui, "Try again", WHAT).invoke_accessible_default_action();
    wait_until(&format!("{WHAT}: expired.bin finished"), || {
        ui.get_finished_rows()
            .iter()
            .any(|r| r.name == "expired.bin" && r.state == "done")
    });
    let file = ctx.out().join("expired.bin");
    assert_eq!(
        std::fs::metadata(&file).map(|m| m.len()).ok(),
        Some(SIZE as u64),
        "{WHAT}: {} not saved in full",
        file.display()
    );
}

/// A link already in the list is flagged on the Add screen and cannot be
/// added twice.
fn duplicate_link_is_flagged(ui: &AppWindow, ctx: &Ctx) {
    const WHAT: &str = "duplicate_link_is_flagged";
    let url = ctx.server.serve("dup.bin", FileSpec::new(64 * 1024));
    add_link(ui, &url, "dup.bin", WHAT);
    let listed = || {
        ui.get_downloading_rows()
            .iter()
            .chain(ui.get_finished_rows().iter())
            .any(|r| r.name == "dup.bin")
    };
    wait_until(&format!("{WHAT}: dup.bin listed"), listed);
    let jobs = || {
        ctx.engine
            .snapshot()
            .iter()
            .filter(|v| v.url == url)
            .count()
    };
    assert_eq!(jobs(), 1, "{WHAT}: first add");

    ui.invoke_link_entered(url.as_str().into());
    assert_eq!(ui.get_screen(), Screen::Add, "{WHAT}: not on Add");
    let add = ui.get_add();
    assert!(add.duplicate, "{WHAT}: not flagged: {add:?}");
    assert_eq!(add.name.as_str(), "dup.bin", "{WHAT}: {add:?}");
    // Confirming anyway adds nothing.
    ui.invoke_add_confirmed();
    assert_eq!(ui.get_screen(), Screen::Add, "{WHAT}: left Add");
    assert_eq!(jobs(), 1, "{WHAT}: a second job was added");
    only_button(ui, "Cancel", WHAT).invoke_accessible_default_action();
    assert_ne!(ui.get_screen(), Screen::Add, "{WHAT}: Cancel");
    ui.set_screen(Screen::Downloads);
}

/// The seconds in "... Trying again in 29 s." texts.
fn retry_seconds_shown(ui: &AppWindow) -> Vec<u32> {
    texts_containing(ui, "Trying again in")
        .iter()
        .filter_map(|t| {
            let rest = t.rsplit("Trying again in ").next()?;
            rest.strip_suffix(" s.")?.parse().ok()
        })
        .collect()
}

/// A download the server keeps refusing with 503 waits, and its row counts
/// down to the next attempt.
fn waiting_row_counts_down() {
    const WHAT: &str = "waiting_row_counts_down";
    let ui = AppWindow::new().unwrap();
    // Fast segment retries, then a long wait before the next attempt, so
    // the row stays Waiting while the clock moves.
    let ctx = Ctx::start_with(&ui, runtime(), |config, _| {
        config.timing = Timing {
            waiting_retry_first: Duration::from_secs(30),
            waiting_retry_cap: Duration::from_secs(30),
            ..Timing::fast_for_tests()
        };
    });
    let mut spec = FileSpec::new(1024 * 1024);
    // The probe at add succeeds; every request after it gets 503.
    spec.fail_first = [vec![0], vec![503; 1000]].concat();
    let url = ctx.server.serve("busy.bin", spec);
    let request = AddRequest {
        url,
        dest_dir: ctx.out(),
        ..Default::default()
    };
    ctx.rt.block_on(ctx.engine.add(request)).unwrap();
    ui.set_tab(0);
    ui.set_screen(Screen::Downloads);
    let mut first = 0;
    wait_until(&format!("{WHAT}: \"Trying again in\" shown"), || {
        let shown = retry_seconds_shown(&ui);
        first = shown.first().copied().unwrap_or(0);
        first > 1
    });
    wait_until(&format!("{WHAT}: counts down from {first} s"), || {
        retry_seconds_shown(&ui).iter().any(|&s| s < first)
    });
    let row = ui.get_downloading_rows().row_data(0).unwrap();
    assert_eq!(
        (row.name.as_str(), row.state.as_str()),
        ("busy.bin", "waiting"),
        "{WHAT}: {row:?}"
    );
    ctx.shutdown(WHAT);
}

/// A torrent added through the Add screen keeps sharing once downloaded
/// (no ratio limit), and its row says so.
fn torrent_row_shows_sharing() {
    const WHAT: &str = "torrent_row_shows_sharing";
    let rt = runtime();
    let data = taurus_testkit::content(&FileSpec::new(256 * 1024));
    let seeder = rt.block_on(TorrentSeeder::start(
        "share.bin",
        &[("share.bin", data)],
        None,
    ));
    let peer = seeder.addr;
    let ui = AppWindow::new().unwrap();
    let ctx = Ctx::start_with(&ui, rt, |config, settings| {
        config.torrent_test = Some(TorrentTestNet {
            initial_peers: vec![peer],
            restore_in_place: false,
        });
        settings.torrent.stop_ratio = None;
    });
    ui.invoke_link_entered(seeder.magnet.as_str().into());
    assert_eq!(ui.get_screen(), Screen::Add, "{WHAT}: not on Add");
    // Torrent metadata comes from a real local peer: slow CI gets time.
    wait_for(
        Duration::from_secs(60),
        &format!("{WHAT}: Add screen shows the torrent"),
        || {
            let add = ui.get_add();
            !add.loading && add.is_torrent
        },
    );
    let add = ui.get_add();
    assert_eq!(add.error_kind.as_str(), "", "{WHAT}: {add:?}");
    ui.invoke_add_confirmed();
    assert_eq!(
        ui.get_screen(),
        Screen::Downloads,
        "{WHAT}: not on Downloads"
    );
    wait_for(Duration::from_secs(60), &format!("{WHAT}: sharing"), || {
        !texts_containing(&ui, "Sharing · ratio ").is_empty()
    });
    let row = ui.get_downloading_rows().row_data(0).unwrap();
    assert_eq!(
        (row.kind.as_str(), row.state.as_str()),
        ("magnet", "seeding"),
        "{WHAT}: {row:?}"
    );
    ctx.shutdown(WHAT);
    drop(seeder);
}

/// With "when done: sleep", finishing the downloads shows the countdown;
/// its Cancel hides it and the engine cancels the action.
fn countdown_cancel() {
    const WHAT: &str = "countdown_cancel";
    let ui = AppWindow::new().unwrap();
    // A long countdown, so it is still running when Cancel is pressed.
    let ctx = Ctx::start_with(&ui, runtime(), |config, _| {
        config.power = PowerMode::DryRun;
        config.timing = Timing {
            when_done_countdown: Duration::from_secs(60),
            ..Timing::fast_for_tests()
        };
    });
    let mut events = ctx.engine.subscribe();
    ui.invoke_setting_changed("when-done".into(), "sleep".into());
    assert_eq!(
        ui.get_settings().when_done.as_str(),
        "sleep",
        "{WHAT}: echo"
    );
    wait_until(&format!("{WHAT}: sleep saved"), || {
        ctx.engine.settings().when_done == WhenDone::Sleep
    });
    let url = ctx.server.serve("last.bin", FileSpec::new(64 * 1024));
    add_link(&ui, &url, "last.bin", WHAT);
    wait_until(&format!("{WHAT}: countdown shown"), || {
        let countdown = ui.get_countdown();
        countdown.visible && countdown.action == "sleep" && countdown.seconds > 0
    });
    assert_eq!(ui.get_finished_rows().row_count(), 1, "{WHAT}: last.bin");

    only_button(&ui, "Cancel", WHAT).invoke_accessible_default_action();
    assert!(!ui.get_countdown().visible, "{WHAT}: still shown");
    let mut cancelled = false;
    wait_until(&format!("{WHAT}: WhenDoneCancelled"), || {
        loop {
            match events.try_recv() {
                Ok(Event::WhenDoneCancelled) => cancelled = true,
                Ok(Event::WhenDoneFired { action, .. }) => {
                    panic!("{WHAT}: {action:?} fired")
                }
                Ok(_) | Err(TryRecvError::Lagged(_)) => {}
                Err(TryRecvError::Empty | TryRecvError::Closed) => break,
            }
        }
        cancelled
    });
    // Later flushes (countdown ticks sent before the cancel) end hidden.
    settle(Duration::from_millis(500));
    assert!(!ui.get_countdown().visible, "{WHAT}: shown again");
    ctx.shutdown(WHAT);
}
