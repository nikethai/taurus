//! UI tests run against Slint's headless testing backend. The backend can be
//! initialized only once per process, so this file holds exactly one `#[test]`;
//! later tasks add named helper functions and call them, in order, from it.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use i_slint_backend_testing::{AccessibleRole, ElementHandle, ElementQuery};
use serde_json::{Value, json};
use slint::platform::WindowEvent;
use slint::{ComponentHandle, Model, ModelRc, VecModel};
use taurus_core::tools::ToolSources;
use taurus_core::torrent::hub::TorrentTestNet;
use taurus_core::{
    AddRequest, Engine, EngineConfig, ErrorKind, Event, JobId, JobState, PowerMode, Settings,
    SpeedLimit, ThemeChoice, Timing, UiMode, WhenDone,
};
use taurus_desktop::app::App;
use taurus_desktop::shell_text::simple_message;
use taurus_desktop::{AppWindow, BannerVm, Messages, RowVm, Screen, SettingsVm, Theme};
use taurus_testkit::{
    FailAfter, FakeToolHost, FileSpec, TestServer, ToolRelease, TorrentSeeder, process_args_log,
    set_ytdlp_scenario, unique_version,
};
use tokio::sync::broadcast::error::TryRecvError;

#[test]
fn ui_smoke() {
    i_slint_backend_testing::init_integration_test_with_system_time();
    let ui = AppWindow::new().unwrap();
    window_title(&ui);
    theme_defaults(&ui);
    welcome_has_paste_field(&ui);
    downloads_rows_show_status_and_retry(&ui);
    rows_banner_and_tips_wording(&ui);
    shell_text_mirrors_messages(&ui);
    media_rows_and_tools_banner_wording(&ui);
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
    let media = MediaCtx::start();
    tools_offline_shows_failed_banner(&media);
    media_add_picks_quality_and_finishes(&media);
    live_recording_shows_and_stops(&media);
    large_playlist_asks_first(&media);
    media.shutdown();
    let adv = advanced_mode_is_compact_with_tab_counts(&ui, &ctx);
    mode_switch_changes_nothing(&ui, &ctx);
    wide_switch_keeps_selection(&ui, &ctx, adv);
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

/// Words the screens build from numbers and flags: an overdue retry, finish
/// times, the copied-link offer for a magnet link, and Welcome tips that
/// promise only what the app does.
fn rows_banner_and_tips_wording(ui: &AppWindow) {
    const WHAT: &str = "rows_banner_and_tips_wording";
    let visible = |label: &str| {
        assert!(
            labelled(ui, label).iter().any(|e| e.size().width > 0.0),
            "{WHAT}: no visible text {label:?}"
        );
    };
    let overdue = RowVm {
        retry_seconds: 0,
        ..row(4, "waiting")
    };
    ui.set_downloading_rows(ModelRc::new(VecModel::from(vec![overdue])));
    ui.set_tab(0);
    ui.set_screen(Screen::Downloads);
    visible("Waiting for a connection. Trying again in a moment.");

    let today = RowVm {
        finished_today: true,
        finished_month: 11,
        finished_day: 1,
        finished_hour: 9,
        finished_minute: 5,
        ..row(5, "done")
    };
    let earlier = RowVm {
        finished_month: 10,
        finished_day: 28,
        finished_hour: 23,
        ..row(6, "done")
    };
    ui.set_finished_rows(ModelRc::new(VecModel::from(vec![
        today,
        earlier,
        row(7, "done"),
    ])));
    ui.set_tab(1);
    visible("400 MB · 09:05");
    visible("400 MB · Oct 28");
    visible("400 MB");

    ui.set_banner(BannerVm {
        visible: true,
        host: "".into(),
        kind: "magnet".into(),
    });
    visible("Download the torrent link you copied?");
    ui.set_banner(BannerVm {
        visible: true,
        host: "cdn.example.org".into(),
        kind: "music".into(),
    });
    visible("Download the link you copied from cdn.example.org?");
    ui.set_banner(BannerVm::default());

    ui.set_screen(Screen::Welcome);
    assert!(
        labelled(ui, "Or drag a link onto this window").is_empty(),
        "{WHAT}: drag tip shown"
    );
    let tip = "Copy a link and Hyades offers to download it";
    let offers = |on: bool| {
        ui.set_settings(SettingsVm {
            offer_copied_links: on,
            ..ui.get_settings()
        })
    };
    offers(true);
    visible(tip);
    offers(false);
    assert!(
        labelled(ui, tip).iter().all(|e| e.size().width == 0.0),
        "{WHAT}: copy tip shown while offers are off"
    );
    ui.set_finished_rows(ModelRc::default());
    ui.set_downloading_rows(ModelRc::default());
    ui.set_tab(0);
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
    settle(Duration::from_millis(500));
    assert_eq!(state(), Some(JobState::Downloading), "{WHAT}: job stopped");
    let rows = ui.get_downloading_rows();
    assert!(
        rows.iter()
            .any(|r| r.name == "tray.bin" && r.state == "downloading"),
        "{WHAT}: hidden window no longer updated"
    );
    // A link copied while the window is in the tray is not offered...
    app.sender()
        .clipboard("https://example.org/hidden.iso".into());
    settle(Duration::from_millis(300));
    assert!(!ui.get_banner().visible, "{WHAT}: offered while hidden");
    // ...one copied once it is back is.
    app.show_window();
    app.sender()
        .clipboard("https://example.org/shown.iso".into());
    wait_until(&format!("{WHAT}: offer once shown"), || {
        ui.get_banner().visible
    });
    assert_eq!(ui.get_banner().host.as_str(), "example.org", "{WHAT}");
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
    // The Add screen's look at the link (request 1) and the engine's probe
    // when adding it (request 2) succeed; the download then gets 404.
    spec.fail_after = Some(FailAfter {
        after_requests: 2,
        status: 404,
        headers: vec![],
    });
    let url = ctx.server.serve("expired.bin", spec);
    ui.invoke_link_entered(url.as_str().into());
    wait_until(&format!("{WHAT}: Add screen shows expired.bin"), || {
        let add = ui.get_add();
        !add.loading && add.name == "expired.bin"
    });
    assert_eq!(
        ctx.server.requests("expired.bin"),
        1,
        "{WHAT}: looking at the link"
    );
    ui.invoke_add_confirmed();
    assert_eq!(
        ui.get_screen(),
        Screen::Downloads,
        "{WHAT}: not on Downloads"
    );
    wait_until(&format!("{WHAT}: expired.bin failed"), || {
        ui.get_downloading_rows()
            .iter()
            .any(|r| r.name == "expired.bin" && r.state == "failed")
    });
    // Probe at add, then the download itself (one request or more).
    let requests = ctx.server.requests("expired.bin");
    assert!(requests >= 3, "{WHAT}: {requests} requests");
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
    let file = ctx.documents().join("expired.bin");
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

/// What media rows, the tools banner and the media Add screen say, from
/// the numbers and flags the app hands them.
fn media_rows_and_tools_banner_wording(ui: &AppWindow) {
    const WHAT: &str = "media_rows_and_tools_banner_wording";
    let visible = |label: &str| {
        assert!(
            labelled(ui, label).iter().any(|e| e.size().width > 0.0),
            "{WHAT}: no visible text {label:?}"
        );
    };
    let media = |id, state: &str, phase: &str| RowVm {
        kind: "media".into(),
        phase: phase.into(),
        ..row(id, state)
    };
    let recording = RowVm {
        kind: "live".into(),
        live_elapsed: "1:05".into(),
        ..media(10, "recording", "downloading")
    };
    let playlist = RowVm {
        entries_done: 2,
        entries_total: 5,
        ..media(13, "downloading", "downloading")
    };
    let waiting_stream = RowVm {
        kind: "live".into(),
        ..media(14, "recording", "waiting-for-stream")
    };
    let rows = vec![
        recording,
        media(11, "downloading", "merging"),
        media(12, "downloading", "getting-tools"),
        playlist,
    ];
    ui.set_downloading_rows(ModelRc::new(VecModel::from(rows)));
    ui.set_tab(0);
    ui.set_screen(Screen::Downloads);
    visible("Recording live · 1:05");
    visible("Finishing up…");
    visible("Getting video tools…");
    visible("2 of 5 videos");
    let stop = only_button(ui, "Stop", WHAT);
    let stopped = Rc::new(RefCell::new(Vec::new()));
    let sink = stopped.clone();
    ui.on_stop_recording(move |id| sink.borrow_mut().push(id));
    stop.invoke_accessible_default_action();
    assert_eq!(*stopped.borrow(), vec![10], "{WHAT}: stop-recording(id)");
    let tools_wait = RowVm {
        retry_seconds: 45,
        ..media(15, "waiting", "getting-tools")
    };
    ui.set_downloading_rows(ModelRc::new(VecModel::from(vec![
        waiting_stream,
        tools_wait,
    ])));
    visible("Waiting for the stream to start");
    visible("Getting video tools… Trying again in 45 s.");

    ui.set_tools_banner("installing".into());
    ui.set_tools_percent(42);
    visible("Getting video tools… 42%");
    ui.set_tools_percent(-1);
    assert!(
        labelled(ui, "Getting video tools…")
            .iter()
            .any(|e| e.size().width > 0.0 && e.accessible_role() == Some(AccessibleRole::Text)),
        "{WHAT}: banner without a percent"
    );
    ui.set_tools_banner("failed".into());
    visible("Couldn't get the video tools. Hyades will try again.");
    ui.set_tools_banner("".into());
    ui.set_downloading_rows(ModelRc::default());

    let video = taurus_desktop::AddVm {
        is_media: true,
        kind: "media".into(),
        name: "A Fine Video".into(),
        uploader: "Some Channel".into(),
        duration_text: "3:32".into(),
        media_mode: "video".into(),
        quality_id: "best".into(),
        quality_label: "Best available".into(),
        size_text: "53 MB".into(),
        ..Default::default()
    };
    ui.set_add(video.clone());
    ui.set_screen(Screen::Add);
    visible("Some Channel · 3:32");
    visible("Best available");
    visible("About 53 MB");
    only_button(ui, "Download", WHAT);
    ui.set_add(taurus_desktop::AddVm {
        live: "live".into(),
        ..video.clone()
    });
    visible("Live now");
    only_button(ui, "Record", WHAT);
    ui.set_add(taurus_desktop::AddVm {
        live: "upcoming".into(),
        starts_at: "14:05".into(),
        ..video.clone()
    });
    visible("Starts at 14:05");
    ui.set_add(taurus_desktop::AddVm {
        playlist_count: 51,
        ..video.clone()
    });
    visible("Playlist · 51 videos");
    ui.set_add(taurus_desktop::AddVm {
        playlist_count: 51,
        needs_confirm: true,
        ..video
    });
    visible("This playlist has 51 videos. Download all of them?");
    only_button(ui, "Download all", WHAT);
    ui.set_add(Default::default());
    ui.set_screen(Screen::Welcome);
}

const VIDEO_LINK: &str = "https://www.youtube.com/watch?v=x";
const LIVE_LINK: &str = "https://www.twitch.tv/hyades_live";
const PLAYLIST_LINK: &str = "https://www.youtube.com/playlist?list=hyades51";

/// The fake yt-dlp of the media checks. Inspections (`-J`) print the
/// link's info; download runs are told apart by their arguments: a live
/// recording (`--live-from-start`), a single video (`--no-playlist`), else
/// a playlist.
fn media_scenario() -> Value {
    let video = json!({
        "id": "abc123",
        "title": "A Fine Video",
        "webpage_url": VIDEO_LINK,
        "extractor_key": "Youtube",
        "uploader": "Some Channel",
        "duration": 212.0,
        "live_status": "not_live",
        "formats": [
            {"format_id": "140", "ext": "m4a", "vcodec": "none", "acodec": "mp4a.40.2", "filesize": 3000000},
            {"format_id": "247", "ext": "webm", "vcodec": "vp9", "acodec": "none", "height": 720, "filesize": 20000000},
            {"format_id": "137", "ext": "mp4", "vcodec": "avc1", "acodec": "none", "height": 1080, "filesize": 50000000}
        ]
    });
    let live = json!({
        "id": "live1",
        "title": "Live Now",
        "webpage_url": LIVE_LINK,
        "extractor_key": "TwitchStream",
        "live_status": "is_live",
        "is_live": true,
        "formats": [
            {"format_id": "720p60", "ext": "mp4", "vcodec": "avc1", "acodec": "mp4a.40.2", "height": 720}
        ]
    });
    let entries: Vec<Value> = (0..51)
        .map(|i| json!({"_type": "url", "id": format!("v{i}"), "title": format!("V{i}")}))
        .collect();
    let playlist = json!({
        "_type": "playlist",
        "id": "hyades51",
        "title": "Big List",
        "entries": entries,
    });
    let progress = |done: u64, total: u64| {
        let p = json!({"status": "downloading", "downloaded_bytes": done, "total_bytes": total, "speed": 1000.0});
        format!("TAURUS-PROGRESS {p}")
    };
    let file = |name: &str| format!("TAURUS-FILE {}", json!(name));
    let song = "A Fine Video [abc123].mp3";
    let first = "Big List/001 - V0 [v0].mp4";
    // Written as the recording starts, so it is there on every platform
    // (on Windows the stop kills the fake rather than interrupting it). As
    // real yt-dlp records a live stream (through ffmpeg): the entry with
    // the file it goes to, then no progress at all.
    let part = "Live Now [live1].mp4.part";
    json!([
        {
            "if_arg": "--live-from-start",
            "stdout": [format!(
                "TAURUS-ENTRY 0 1 {} {} {}",
                json!("live1"),
                json!("Live Now"),
                json!("Live Now [live1].mp4")
            )],
            "start_files": [[part, 4096]],
            "wait_for_interrupt": true,
            "on_interrupt": {"stdout": ["bye"], "exit": 1},
        },
        {
            "if_arg": "--no-playlist",
            "stdout": [progress(150, 300), progress(300, 300), file(song)],
            "delay_ms_per_line": 30,
            "write_files": [[song, 300]],
        },
        {
            "if_arg": "--progress",
            "stdout": [progress(10, 10), file(first)],
            "write_files": [[first, 10]],
        },
        {"if_arg": LIVE_LINK, "stdout": [live.to_string()]},
        {"if_arg": PLAYLIST_LINK, "stdout": [playlist.to_string()]},
        {"stdout": [video.to_string()]},
    ])
}

/// An app whose engine gets its video tools from a local fake publisher.
struct MediaCtx {
    ui: AppWindow,
    host: FakeToolHost,
    ctx: Ctx,
}

impl MediaCtx {
    fn start() -> MediaCtx {
        let version = unique_version();
        set_ytdlp_scenario(&version, &media_scenario().to_string());
        let rt = runtime();
        let host = rt.block_on(FakeToolHost::start(
            ToolRelease::new(&version),
            ToolRelease::new("7.1"),
            ToolRelease::new("2.5.0"),
        ));
        let base = url::Url::parse(&host.base_url()).unwrap();
        let ui = AppWindow::new().unwrap();
        let ctx = Ctx::start_with(&ui, rt, |config, _| {
            config.tool_sources = ToolSources::under(&base);
            config.timing = Timing::fast_for_tests();
        });
        MediaCtx { ui, host, ctx }
    }

    fn shutdown(self) {
        self.ctx.shutdown("media");
        drop(self.host);
    }

    fn videos(&self) -> PathBuf {
        self.ctx.out().join("Videos")
    }

    fn music(&self) -> PathBuf {
        self.ctx.out().join("Music")
    }
}

/// Installing tools takes a while on a slow machine.
const TOOLS_WAIT: Duration = Duration::from_secs(60);

/// No tools yet and their publisher unreachable: looking at a video link
/// fails, and the banner says the tools could not be fetched.
fn tools_offline_shows_failed_banner(m: &MediaCtx) {
    const WHAT: &str = "tools_offline_shows_failed_banner";
    let ui = &m.ui;
    m.host.go_offline();
    ui.invoke_link_entered(VIDEO_LINK.into());
    assert_eq!(ui.get_screen(), Screen::Add, "{WHAT}: not on Add");
    wait_for(TOOLS_WAIT, &format!("{WHAT}: banner says failed"), || {
        ui.get_tools_banner() == "failed"
    });
    wait_until(&format!("{WHAT}: the Add screen gives up"), || {
        let add = ui.get_add();
        !add.loading && !add.error_kind.is_empty()
    });
    ui.invoke_add_cancelled();
    m.host.go_online();
}

/// A video link: Video / Audio only, the quality picked by cycling, the
/// "Save to" folder following the mode, and the download finishing.
fn media_add_picks_quality_and_finishes(m: &MediaCtx) {
    const WHAT: &str = "media_add_picks_quality_and_finishes";
    let ui = &m.ui;
    ui.invoke_link_entered(VIDEO_LINK.into());
    assert_eq!(ui.get_screen(), Screen::Add, "{WHAT}: not on Add");
    wait_for(
        TOOLS_WAIT,
        &format!("{WHAT}: Add screen shows the video"),
        || {
            let add = ui.get_add();
            !add.loading && add.is_media
        },
    );
    let add = ui.get_add();
    assert_eq!(add.error_kind.as_str(), "", "{WHAT}: {add:?}");
    assert_eq!(
        (
            add.name.as_str(),
            add.kind.as_str(),
            add.media_mode.as_str(),
            add.quality_id.as_str(),
            add.quality_label.as_str(),
        ),
        ("A Fine Video", "media", "video", "best", "Best available"),
        "{WHAT}: {add:?}"
    );
    assert_eq!(
        (add.uploader.as_str(), add.duration_text.as_str()),
        ("Some Channel", "3:32"),
        "{WHAT}"
    );
    assert_eq!(add.size_text.as_str(), "53 MB", "{WHAT}: estimate");
    assert_eq!(PathBuf::from(add.folder.as_str()), m.videos(), "{WHAT}");
    // The tools are in: the banner is gone.
    wait_until(&format!("{WHAT}: banner cleared"), || {
        ui.get_tools_banner() == ""
    });
    ui.invoke_quality_cycled();
    assert_eq!(ui.get_add().quality_label.as_str(), "1080p", "{WHAT}");

    ui.invoke_media_mode_changed("audio".into());
    let add = ui.get_add();
    assert_eq!(
        (
            add.media_mode.as_str(),
            add.kind.as_str(),
            add.quality_label.as_str()
        ),
        ("audio", "music", "Best"),
        "{WHAT}: {add:?}"
    );
    assert_eq!(PathBuf::from(add.folder.as_str()), m.music(), "{WHAT}");
    ui.invoke_quality_cycled();
    assert_eq!(
        ui.get_add().quality_label.as_str(),
        "Smaller file",
        "{WHAT}"
    );
    ui.invoke_add_confirmed();
    assert_eq!(
        ui.get_screen(),
        Screen::Downloads,
        "{WHAT}: not on Downloads"
    );
    wait_for(TOOLS_WAIT, &format!("{WHAT}: the song finished"), || {
        ui.get_finished_rows()
            .iter()
            .any(|r| r.name == "A Fine Video [abc123].mp3" && r.state == "done")
    });
    let row = ui
        .get_finished_rows()
        .iter()
        .find(|r| r.name == "A Fine Video [abc123].mp3")
        .unwrap();
    assert_eq!(row.kind.as_str(), "music", "{WHAT}: {row:?}");
    assert!(
        m.music().join("A Fine Video [abc123].mp3").is_file(),
        "{WHAT}: not saved in Music"
    );
    // The smaller file is an MP3.
    let runs: Vec<Vec<String>> = process_args_log()
        .into_iter()
        .filter(|e| e.tool == "yt-dlp" && e.argv.iter().any(|a| a == VIDEO_LINK))
        .filter(|e| !e.argv.iter().any(|a| a == "-J"))
        .map(|e| e.argv)
        .collect();
    assert_eq!(runs.len(), 1, "{WHAT}: {runs:?}");
    assert!(
        runs[0].windows(2).any(|w| w == ["--audio-format", "mp3"]),
        "{WHAT}: {:?}",
        runs[0]
    );
}

/// A live stream: "Live now" and Record on the Add screen, a recording row
/// that says so, and Stop keeping what was recorded.
fn live_recording_shows_and_stops(m: &MediaCtx) {
    const WHAT: &str = "live_recording_shows_and_stops";
    let ui = &m.ui;
    ui.invoke_link_entered(LIVE_LINK.into());
    wait_until(&format!("{WHAT}: Add screen shows the stream"), || {
        let add = ui.get_add();
        !add.loading && add.is_media
    });
    let add = ui.get_add();
    assert_eq!(
        (add.live.as_str(), add.kind.as_str()),
        ("live", "live"),
        "{WHAT}: {add:?}"
    );
    assert_eq!(PathBuf::from(add.folder.as_str()), m.videos(), "{WHAT}");
    only_button(ui, "Record", WHAT).invoke_accessible_default_action();
    assert_eq!(
        ui.get_screen(),
        Screen::Downloads,
        "{WHAT}: not on Downloads"
    );
    wait_until(&format!("{WHAT}: recording row"), || {
        ui.get_downloading_rows()
            .iter()
            .any(|r| r.state == "recording" && r.kind == "live")
            && !texts_containing(ui, "Recording live").is_empty()
    });
    only_button(ui, "Stop", WHAT).invoke_accessible_default_action();
    wait_until(&format!("{WHAT}: the recording finished"), || {
        ui.get_finished_rows()
            .iter()
            .any(|r| r.name == "Live Now [live1].mp4" && r.state == "done")
    });
    assert!(
        m.videos().join("Live Now [live1].mp4").is_file(),
        "{WHAT}: the recording was not kept"
    );
}

/// A playlist of more than 50 videos asks before downloading them all.
fn large_playlist_asks_first(m: &MediaCtx) {
    const WHAT: &str = "large_playlist_asks_first";
    let ui = &m.ui;
    ui.invoke_link_entered(PLAYLIST_LINK.into());
    wait_until(&format!("{WHAT}: Add screen shows the playlist"), || {
        let add = ui.get_add();
        !add.loading && add.is_media
    });
    assert_eq!(ui.get_add().playlist_count, 51, "{WHAT}");
    ui.invoke_add_confirmed();
    wait_until(&format!("{WHAT}: asks first"), || {
        ui.get_add().needs_confirm
    });
    assert_eq!(ui.get_screen(), Screen::Add, "{WHAT}: left Add");
    let jobs = || {
        m.ctx
            .engine
            .snapshot()
            .iter()
            .filter(|v| v.url == PLAYLIST_LINK)
            .count()
    };
    assert_eq!(jobs(), 0, "{WHAT}: added without asking");
    ui.invoke_playlist_confirmed();
    assert_eq!(
        ui.get_screen(),
        Screen::Downloads,
        "{WHAT}: not on Downloads"
    );
    wait_until(&format!("{WHAT}: the playlist finished"), || {
        ui.get_finished_rows()
            .iter()
            .any(|r| r.name == "Big List" && r.state == "done")
    });
    assert_eq!(jobs(), 1, "{WHAT}");
}

/// The compact window, and one tall enough to show all of Settings.
const COMPACT: slint::LogicalSize = slint::LogicalSize::new(420.0, 640.0);
const TALL: slint::LogicalSize = slint::LogicalSize::new(420.0, 1600.0);

/// The one element with role `role` labelled `label`.
fn only_with_role(ui: &AppWindow, role: AccessibleRole, label: &str, what: &str) -> ElementHandle {
    let mut found: Vec<ElementHandle> = labelled(ui, label)
        .into_iter()
        .filter(|e| e.accessible_role() == Some(role))
        .collect();
    assert_eq!(
        found.len(),
        1,
        "{what}: expected one {role:?} labelled {label:?}, found {}",
        found.len()
    );
    found.remove(0)
}

/// Active / Waiting / Done counts of `views`, by the compact tab rules.
fn tab_counts_of(views: &[taurus_core::JobView]) -> (i32, i32, i32) {
    let n = |states: &[JobState]| views.iter().filter(|v| states.contains(&v.state)).count() as i32;
    (
        n(&[
            JobState::Downloading,
            JobState::Recording,
            JobState::Seeding,
        ]),
        n(&[
            JobState::Queued,
            JobState::Waiting,
            JobState::Paused,
            JobState::Failed,
        ]),
        n(&[JobState::Completed]),
    )
}

/// "Show advanced options" in Simple Settings turns on Advanced mode: in a
/// window narrower than 900 px that is the compact layout, whose Active /
/// Waiting / Done tabs show their counts. Leaves a paused download
/// (returned) for the next checks.
fn advanced_mode_is_compact_with_tab_counts(ui: &AppWindow, ctx: &Ctx) -> JobId {
    const WHAT: &str = "advanced_mode_is_compact_with_tab_counts";
    let mut spec = FileSpec::new(20 * 1024 * 1024);
    spec.bytes_per_sec = Some(256 * 1024);
    let url = ctx.server.serve("adv.bin", spec);
    let request = AddRequest {
        url,
        dest_dir: ctx.out(),
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
    wait_until(&format!("{WHAT}: adv.bin downloading"), || {
        state() == Some(JobState::Downloading)
    });
    ctx.rt.block_on(ctx.engine.pause(id)).unwrap();
    wait_until(&format!("{WHAT}: adv.bin paused"), || {
        ui.get_downloading_rows()
            .iter()
            .any(|r| r.name == "adv.bin" && r.state == "paused")
    });

    // Tall enough that the whole Settings screen is in view (the testing
    // backend finds no element scrolled out of it).
    ui.window().set_size(TALL);
    settle(Duration::from_millis(200));
    assert_eq!(ui.get_layout().as_str(), "simple", "{WHAT}: Simple first");
    ui.set_screen(Screen::Settings);
    let switch = only_with_role(ui, AccessibleRole::Switch, "Show advanced options", WHAT);
    assert_eq!(
        switch.accessible_checked(),
        Some(false),
        "{WHAT}: switch off"
    );
    switch.invoke_accessible_default_action();
    assert_eq!(ui.get_layout().as_str(), "compact", "{WHAT}: layout");
    wait_until(&format!("{WHAT}: ui.mode saved"), || {
        ctx.engine.settings().ui.mode == UiMode::Advanced
    });
    ui.set_screen(Screen::Downloads);
    ui.window().set_size(COMPACT);

    let (active, waiting, done) = tab_counts_of(&ctx.engine.snapshot());
    assert!(
        waiting >= 1 && done >= 1,
        "{WHAT}: {waiting} waiting, {done} done"
    );
    wait_until(&format!("{WHAT}: tab counts"), || {
        let c = ui.get_tab_counts();
        (c.active, c.waiting, c.done) == (active, waiting, done)
    });
    for (label, count) in [("Active", active), ("Waiting", waiting), ("Done", done)] {
        let tab = only_with_role(ui, AccessibleRole::Tab, label, WHAT);
        assert!(tab.size().width > 0.0, "{WHAT}: tab {label} not shown");
        assert_eq!(
            tab.accessible_value().map(|v| v.to_string()),
            Some(count.to_string()),
            "{WHAT}: tab {label} count"
        );
    }
    only_with_role(ui, AccessibleRole::Tab, "Waiting", WHAT).invoke_accessible_default_action();
    wait_until(&format!("{WHAT}: Waiting lists adv.bin"), || {
        ui.get_adv_rows()
            .iter()
            .any(|r| r.row.name == "adv.bin" && r.row.state == "paused" && r.source == "HTTP")
    });
    assert_eq!(
        ui.get_adv_rows().row_count() as i32,
        waiting,
        "{WHAT}: Waiting rows"
    );
    id
}

/// Turning Advanced on and off changes no job and no engine setting but
/// `ui.mode`; Simple shows a custom speed (set in Advanced) as "Custom",
/// read-only: looking at it changes nothing.
fn mode_switch_changes_nothing(ui: &AppWindow, ctx: &Ctx) {
    const WHAT: &str = "mode_switch_changes_nothing";
    let mut custom = ctx.engine.settings();
    custom.speed = SpeedLimit::Custom {
        bytes_per_sec: 3_000_000,
    };
    ctx.rt.block_on(ctx.engine.set_settings(custom)).unwrap();
    wait_until(&format!("{WHAT}: custom speed shown"), || {
        ui.get_settings().speed == "custom"
    });
    let snapshot = || {
        let mut settings = ctx.engine.settings();
        settings.ui.mode = UiMode::Simple;
        (format!("{:?}", ctx.engine.snapshot()), settings)
    };
    let before = snapshot();
    for round in 0..2 {
        for (advanced, mode, layout) in [
            ("false", UiMode::Simple, "simple"),
            ("true", UiMode::Advanced, "compact"),
        ] {
            ui.invoke_setting_changed("advanced".into(), advanced.into());
            assert_eq!(
                ui.get_layout().as_str(),
                layout,
                "{WHAT}: {round} {advanced}"
            );
            wait_until(&format!("{WHAT}: {round} saved {mode:?}"), || {
                ctx.engine.settings().ui.mode == mode
            });
            if round == 0 && mode == UiMode::Simple {
                ui.window().set_size(TALL);
                ui.set_screen(Screen::Settings);
                wait_until(&format!("{WHAT}: Simple shows Custom"), || {
                    labelled(ui, "Custom · 3.0 MB/s")
                        .iter()
                        .any(|e| e.size().width > 0.0)
                });
                settle(Duration::from_millis(300));
                ui.set_screen(Screen::Downloads);
                ui.window().set_size(COMPACT);
            }
        }
    }
    settle(Duration::from_millis(300));
    let after = snapshot();
    assert_eq!(before.1, after.1, "{WHAT}: settings changed");
    assert_eq!(before.0, after.0, "{WHAT}: jobs changed");
    assert_eq!(
        ctx.engine.settings().speed,
        SpeedLimit::Custom {
            bytes_per_sec: 3_000_000
        },
        "{WHAT}: custom speed kept"
    );
}

/// Widening the window past 900 px switches to the wide layout and keeps
/// the selected download, which the detail pane shows; narrowing it back
/// keeps it too. Ends in Simple mode with a preset speed picked there
/// (which replaces the custom one).
fn wide_switch_keeps_selection(ui: &AppWindow, ctx: &Ctx, id: JobId) {
    const WHAT: &str = "wide_switch_keeps_selection";
    assert_eq!(ui.get_layout().as_str(), "compact", "{WHAT}: compact first");
    ui.set_screen(Screen::Downloads);
    only_with_role(ui, AccessibleRole::Tab, "Waiting", WHAT).invoke_accessible_default_action();
    wait_until(&format!("{WHAT}: adv.bin listed"), || {
        !labelled(ui, "adv.bin").is_empty()
    });
    only_with_role(ui, AccessibleRole::ListItem, "adv.bin", WHAT)
        .invoke_accessible_default_action();
    let want = id.0 as i32;
    assert_eq!(ui.get_selected_id(), want, "{WHAT}: selected");
    wait_until(&format!("{WHAT}: row marked selected"), || {
        ui.get_adv_rows()
            .iter()
            .any(|r| r.row.id == want && r.selected)
    });

    ui.window().set_size(slint::LogicalSize::new(1200.0, 640.0));
    wait_until(&format!("{WHAT}: wide"), || ui.get_layout() == "wide");
    assert_eq!(ui.get_selected_id(), want, "{WHAT}: selection kept");
    let detail = ElementHandle::find_by_element_type_name(ui, "DetailStub")
        .next()
        .unwrap_or_else(|| panic!("{WHAT}: no detail pane"));
    assert!(detail.size().width > 0.0, "{WHAT}: detail pane hidden");
    let shown: Vec<String> = detail
        .query_descendants()
        .match_accessible_role(AccessibleRole::Text)
        .find_all()
        .into_iter()
        .filter_map(|e| e.accessible_label().map(|l| l.to_string()))
        .collect();
    assert!(
        shown.iter().any(|t| t == "adv.bin") && shown.iter().any(|t| t == "HTTP"),
        "{WHAT}: detail pane shows {shown:?}"
    );
    // The sidebar's counts and filters work in the wide layout.
    let paused: Vec<i32> = ctx
        .engine
        .snapshot()
        .iter()
        .filter(|v| v.state == JobState::Paused)
        .map(|v| v.id.0 as i32)
        .collect();
    assert!(paused.contains(&want), "{WHAT}: {paused:?}");
    let count = ui
        .get_filter_counts()
        .iter()
        .find(|c| c.key == "paused")
        .map(|c| c.count);
    assert_eq!(count, Some(paused.len() as i32), "{WHAT}: Paused count");
    only_with_role(ui, AccessibleRole::ListItem, "Paused", WHAT).invoke_accessible_default_action();
    assert_eq!(ui.get_filter().as_str(), "paused", "{WHAT}: filter");
    assert_eq!(
        ui.get_adv_rows()
            .iter()
            .map(|r| r.row.id)
            .collect::<Vec<_>>(),
        paused,
        "{WHAT}: Paused filter"
    );

    ui.window().set_size(COMPACT);
    wait_until(&format!("{WHAT}: compact again"), || {
        ui.get_layout() == "compact"
    });
    assert_eq!(ui.get_selected_id(), want, "{WHAT}: selection kept back");

    // Back to Simple; picking a preset there replaces the custom speed.
    ui.window().set_size(TALL);
    ui.set_screen(Screen::Settings);
    only_with_role(ui, AccessibleRole::Switch, "Show advanced options", WHAT)
        .invoke_accessible_default_action();
    assert_eq!(ui.get_layout().as_str(), "simple", "{WHAT}: Simple again");
    only_with_role(ui, AccessibleRole::RadioButton, "Fastest", WHAT)
        .invoke_accessible_default_action();
    wait_until(&format!("{WHAT}: preset saved"), || {
        let s = ctx.engine.settings();
        s.speed == SpeedLimit::Fastest && s.ui.mode == UiMode::Simple
    });
    ui.set_screen(Screen::Downloads);
    ui.window().set_size(COMPACT);
    ctx.rt.block_on(ctx.engine.remove(id, true)).unwrap();
}
