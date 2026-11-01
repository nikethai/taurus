//! UI tests run against Slint's headless testing backend. The backend can be
//! initialized only once per process, so this file holds exactly one `#[test]`;
//! later tasks add named helper functions and call them, in order, from it.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use i_slint_backend_testing::{AccessibleRole, ElementHandle};
use slint::{ComponentHandle, Model, ModelRc, VecModel};
use taurus_core::{Engine, EngineConfig, ErrorKind, JobState, ThemeChoice};
use taurus_desktop::app::App;
use taurus_desktop::shell_text::simple_message;
use taurus_desktop::{AppWindow, Messages, RowVm, Screen, Theme};
use taurus_testkit::{FileSpec, TestServer};

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
    quit_shuts_down_engine_cleanly(&ui, &ctx);
    // later tasks append their checks here
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

impl Ctx {
    fn start(ui: &AppWindow) -> Ctx {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let server = rt.block_on(TestServer::start());
        let engine = rt
            .block_on(Engine::open(EngineConfig::new(tmp.path().join("data"))))
            .unwrap();
        let out = tmp.path().join("out");
        std::fs::create_dir_all(&out).unwrap();
        let mut settings = engine.settings();
        settings.ui.folder = Some(out);
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
fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
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
