//! UI tests run against Slint's headless testing backend. The backend can be
//! initialized only once per process, so this file holds exactly one `#[test]`;
//! later tasks add named helper functions and call them, in order, from it.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use i_slint_backend_testing::{AccessibleRole, ElementHandle, ElementQuery};
use serde_json::{Value, json};
use slint::platform::WindowEvent;
use slint::{ComponentHandle, Model, ModelRc, VecModel};
use taurus_core::secrets::MemoryStore;
use taurus_core::tools::ToolSources;
use taurus_core::torrent::hub::TorrentTestNet;
use taurus_core::{
    AddRequest, Engine, EngineConfig, ErrorKind, Event, JobId, JobState, PowerMode, ProxySetting,
    Settings, SpeedLimit, ThemeChoice, Timing, UiMode, WhenDone,
};
use taurus_desktop::app::App;
use taurus_desktop::i18n;
use taurus_desktop::rows;
use taurus_desktop::shell_text::ShellStrings;
use taurus_desktop::{
    AdvRowVm, AppWindow, BannerVm, CountdownVm, DetailVm, FilesDialogVm, LinksUi, PasswordDialogVm,
    RemoveDialogVm, RowVm, Screen, SettingsUi, SettingsVm, Theme, TorrentFileVm, WhenDoneUi,
};
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
    system_theme_follows_os();
    welcome_has_paste_field(&ui);
    bundled_pseudo_locale_applies(&ui);
    shell_text_follows_language(&ui);
    downloads_rows_show_status_and_retry(&ui);
    rows_banner_and_tips_wording(&ui);
    shell_simple_message_matches_window(&ui);
    media_rows_and_tools_banner_wording(&ui);
    keyboard_tab_enter_escape();
    wide_detail_holds_long_names();
    torrent_add_scrolls_only_its_list();
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
    convert_shows_media_time(&media);
    adv_add_media_options(&media);
    adv_settings_engines_roll_back(&media);
    media.shutdown();
    let adv = advanced_mode_is_compact_with_tab_counts(&ui, &ctx);
    mode_switch_changes_nothing(&ui, &ctx);
    http_detail_shows_cells(&ui, &ctx, adv);
    remove_dialog_defaults_to_keep(&ui, &ctx);
    password_cleared_after_submit(&ui, &ctx);
    wide_switch_keeps_selection(&ui, &ctx, adv);
    adv_add_file_mirrors_checksum(&ui, &ctx);
    adv_add_password_never_lingers(&ui, &ctx);
    grab_listing_chip_counts(&ui, &ctx);
    grab_download_adds_three(&ui, &ctx);
    grab_keeps_subfolders(&ui, &ctx);
    grab_500_links_streams_sizes(&ui, &ctx);
    batch_copies_become_mirrors(&ui, &ctx);
    add_page_offers_find_files(&ui, &ctx);
    whendone_menu_sets_when_done_and_speed(&ui, &ctx);
    adv_settings_sections_and_values(&ui, &ctx);
    torrent_detail_files_and_overrides();
    adv_add_torrent_picker();
    quit_shuts_down_engine_cleanly(&ui, &ctx);
}

fn window_title(ui: &AppWindow) {
    assert_eq!(ui.get_window_title(), "Hyades");
}

fn theme_defaults(ui: &AppWindow) {
    let theme = ui.global::<Theme>();
    let color = |r, g, b| slint::Color::from_rgb_u8(r, g, b);
    let pick = |scheme: &str| {
        theme.set_scheme(scheme.into());
        theme.invoke_sync_palette();
    };
    pick("light");
    assert_eq!(theme.get_bg().color(), color(0xFB, 0xFB, 0xFC));
    pick("dark");
    assert_eq!(theme.get_bg().color(), color(0x1C, 0x1C, 0x1F));
    assert_eq!(theme.get_accent_base().color(), color(0xE8, 0x59, 0x0C));
    pick("system");
}

/// Elements whose accessible label is exactly `label`.
/// With the System theme the app and the std widgets follow the OS scheme
/// live, at launch and after Dark or Light was picked (review N2). The OS
/// scheme is set the way the winit backend's theme observer sets it: Slint
/// has no public API for that, so this uses its private one.
fn system_theme_follows_os() {
    use slint::private_unstable_api::re_exports::{ColorScheme, WindowInner};
    // A fresh window: its theme has never been touched.
    let ui = AppWindow::new().unwrap();
    let theme = ui.global::<Theme>();
    let os = |scheme| {
        WindowInner::from_pub(ui.window())
            .context()
            .set_color_scheme(scheme)
    };
    let pick = |scheme: &str| {
        theme.set_scheme(scheme.into());
        theme.invoke_sync_palette();
    };
    // Launch with Dark saved: settings are pushed before the OS scheme is known.
    pick("dark");
    os(ColorScheme::Light);
    assert!(theme.get_dark(), "Dark forces dark");
    assert!(theme.get_palette_dark(), "Dark forces the std palette dark");
    pick("system");
    assert!(!theme.get_dark(), "Dark -> System on a light OS");
    assert!(!theme.get_palette_dark(), "std palette left forced dark");
    os(ColorScheme::Dark);
    assert!(theme.get_dark(), "System follows the OS turning dark");
    assert!(
        theme.get_palette_dark(),
        "std palette follows the OS turning dark"
    );
    pick("light");
    assert!(!theme.get_dark(), "Light forces light on a dark OS");
    assert!(
        !theme.get_palette_dark(),
        "Light forces the std palette light"
    );
    pick("system");
    assert!(theme.get_dark(), "Light -> System on a dark OS");
    assert!(theme.get_palette_dark(), "std palette left forced light");
    os(ColorScheme::Light);
    assert!(!theme.get_dark(), "System follows the OS turning light");
    assert!(
        !theme.get_palette_dark(),
        "std palette follows the OS turning light"
    );
    // Back to what the testing backend reports: no OS scheme, read as light.
    os(ColorScheme::Unknown);
    assert!(!theme.get_dark(), "an unknown OS scheme reads as light");
}

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

/// The bundled pseudo-locale reaches the window, and selecting `en` brings
/// back the source text. The product name is never translated.
fn bundled_pseudo_locale_applies(ui: &AppWindow) {
    const WHAT: &str = "bundled_pseudo_locale_applies";
    ui.set_screen(Screen::Welcome);
    i18n::apply(Some("en_XA"));
    let tagline = texts_containing(ui, "Dówñlóáds mádé símplé.");
    assert!(!tagline.is_empty(), "{WHAT}: no pseudo tagline");
    assert!(
        tagline.iter().all(|t| t.starts_with('⟦')),
        "{WHAT}: {tagline:?}"
    );
    assert_eq!(ui.get_window_title(), "Hyades", "{WHAT}: title translated");
    // Plurals and placeholders take another path at run time: the number
    // and the argument are filled in, and the catalog's plural rule picks
    // the form.
    let countdown = |seconds| {
        ui.set_countdown(CountdownVm {
            visible: true,
            action: "sleep".into(),
            seconds,
        });
        refresh_tree(ui);
        texts_containing(ui, "Sléépíñg íñ")
    };
    let one = countdown(1);
    assert!(
        !one.is_empty() && one.iter().all(|t| t.starts_with("⟦Sléépíñg íñ 1 séçóñd·")),
        "{WHAT}: singular {one:?}"
    );
    let five = countdown(5);
    assert!(
        !five.is_empty()
            && five
                .iter()
                .all(|t| t.starts_with("⟦Sléépíñg íñ 5 séçóñds·")),
        "{WHAT}: plural {five:?}"
    );
    ui.set_countdown(CountdownVm::default());
    ui.set_remove_dialog(RemoveDialogVm {
        visible: true,
        id: 7,
        name: "a.bin".into(),
    });
    refresh_tree(ui);
    let remove = texts_containing(ui, "Rémóvé a.bin");
    assert!(
        !remove.is_empty()
            && remove
                .iter()
                .all(|t| t.starts_with("⟦Rémóvé a.bin fróm thé líst?·")),
        "{WHAT}: placeholder {remove:?}"
    );
    ui.set_remove_dialog(RemoveDialogVm::default());
    refresh_tree(ui);
    i18n::apply(Some("en"));
    let tagline = texts_containing(ui, "Downloads made simple");
    assert!(!tagline.is_empty(), "{WHAT}: no English tagline");
    assert!(
        tagline.iter().all(|t| t == "Downloads made simple."),
        "{WHAT}: {tagline:?}"
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

/// The text the OS shows outside the window (tray menu, notification
/// titles, start-up message box) comes from the catalog and follows the
/// selected language like the window does.
fn shell_text_follows_language(ui: &AppWindow) {
    const WHAT: &str = "shell_text_follows_language";
    i18n::apply(Some("en_XA"));
    let pseudo = ShellStrings::from_ui(ui);
    for (name, text) in [
        ("tray_open", &pseudo.tray_open),
        ("tray_pause_all", &pseudo.tray_pause_all),
        ("tray_resume_all", &pseudo.tray_resume_all),
        ("tray_quit", &pseudo.tray_quit),
        ("finished_title", &pseudo.finished_title),
        ("failed_title", &pseudo.failed_title),
        ("all_done_title", &pseudo.all_done_title),
        ("open_in_use", &pseudo.open_in_use),
        ("open_failed", &pseudo.open_failed),
    ] {
        assert!(text.starts_with('⟦'), "{WHAT}: {name} {text:?}");
    }
    i18n::apply(Some("en"));
    let en = ShellStrings::from_ui(ui);
    assert_eq!(en.tray_quit, "Quit", "{WHAT}");
    assert_eq!(en.tray_open, "Open Hyades", "{WHAT}");
    assert_eq!(en.tray_pause_all, "Pause all", "{WHAT}");
    assert_eq!(en.tray_resume_all, "Resume all", "{WHAT}");
    assert_eq!(en.finished_title, "Download finished", "{WHAT}");
    assert_eq!(en.failed_title, "Download stopped", "{WHAT}");
    assert_eq!(en.all_done_title, "All downloads finished", "{WHAT}");
    assert_eq!(
        en.open_in_use,
        "Hyades is already running in another program (hyades-cli). Close it, then open Hyades again.",
        "{WHAT}"
    );
    assert_eq!(
        en.open_failed,
        "Hyades couldn't open its data folder. Check that the drive is there and that you can write to it, then open Hyades again.",
        "{WHAT}"
    );
}

/// The failed-download notification says, for every error kind, what the
/// Simple row of that failed download says.
fn shell_simple_message_matches_window(ui: &AppWindow) {
    const WHAT: &str = "shell_simple_message_matches_window";
    ui.set_tab(0);
    ui.set_screen(Screen::Downloads);
    for kind in ErrorKind::ALL {
        let view = taurus_core::JobView {
            id: JobId(1),
            url: "https://example.org/a.bin".into(),
            source_url: "https://example.org/a.bin".into(),
            file_name: "a.bin".into(),
            dest_dir: PathBuf::from("/tmp"),
            state: JobState::Failed,
            total: Some(1000),
            downloaded: 0,
            speed_bps: 0,
            resumable: true,
            error: Some((kind, String::new())),
            kind: taurus_core::JobKind::Http,
            torrent: None,
            retry_at: None,
            finished_at: None,
            info_hash: None,
            media: None,
            progress_unit: Default::default(),
        };
        let row = rows::row_of(&view, 0);
        ui.set_downloading_rows(ModelRc::new(VecModel::from(vec![row])));
        refresh_tree(ui);
        let text = ShellStrings::simple(ui, kind);
        assert!(
            labelled(ui, &text).iter().any(|e| e.size().width > 0.0),
            "{WHAT}: {kind:?}: the row does not show {text:?}"
        );
    }
    ui.set_downloading_rows(ModelRc::default());
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
        // Never the computer's keychain.
        config.secrets = Arc::new(MemoryStore::new());
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
        "subtitles": {"en": [{"ext": "vtt"}], "vi": [{"ext": "vtt"}]},
        "automatic_captions": {"fr": [{"ext": "vtt"}]},
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
/// keeps it too. The search field and the list agree across layout and
/// screen changes; removing the selected download clears the selection.
/// Ends in Simple mode with a preset speed picked there (which replaces
/// the custom one).
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
    // Compact: the detail opens over the list.
    only_with_role(ui, AccessibleRole::Button, "Back", WHAT);
    assert!(
        labelled(ui, "Waiting").is_empty(),
        "{WHAT}: the list is still shown under the detail"
    );

    ui.window().set_size(slint::LogicalSize::new(1200.0, 640.0));
    wait_until(&format!("{WHAT}: wide"), || ui.get_layout() == "wide");
    assert_eq!(ui.get_selected_id(), want, "{WHAT}: selection kept");
    let detail = ElementHandle::find_by_element_type_name(ui, "DetailPane")
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
    let shown = ui.get_detail();
    assert_eq!(
        (shown.visible, shown.name.as_str(), shown.kind.as_str()),
        (true, "adv.bin", "file"),
        "{WHAT}: detail kind"
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

    // A search typed in wide filters the list and outlives its field
    // (Settings and back).
    let field = || only_with_role(ui, AccessibleRole::TextInput, "Search downloads", WHAT);
    let field_text = || field().accessible_value().unwrap_or_default().to_string();
    let ids = || {
        ui.get_adv_rows()
            .iter()
            .map(|r| r.row.id)
            .collect::<Vec<_>>()
    };
    field().set_accessible_value("ADV.B");
    assert_eq!(ids(), [want], "{WHAT}: search");
    field().set_accessible_value("no such file");
    assert!(ids().is_empty(), "{WHAT}: search hides all");
    field().set_accessible_value("adv.b");
    // Element queries walk the tree, which rebuilds what `if` shows: the
    // search field is really gone while Settings is open.
    ui.set_screen(Screen::Settings);
    only_with_role(ui, AccessibleRole::Button, "Back", WHAT);
    assert!(
        labelled(ui, "Search downloads").is_empty(),
        "{WHAT}: search field still there on Settings"
    );
    ui.set_screen(Screen::Downloads);
    assert_eq!(field_text(), "adv.b", "{WHAT}: field after Settings");
    assert_eq!(ids(), [want], "{WHAT}: filtered after Settings");

    // The compact layout has no search field: going compact clears the
    // search, and the tab lists all of its jobs.
    ui.window().set_size(COMPACT);
    wait_until(&format!("{WHAT}: compact again"), || {
        ui.get_layout() == "compact"
    });
    assert_eq!(ui.get_selected_id(), want, "{WHAT}: selection kept back");
    // ...with its detail still open, now over the whole window.
    assert!(ui.get_detail().visible, "{WHAT}: detail closed");
    only_with_role(ui, AccessibleRole::Button, "Back", WHAT);
    assert_eq!(ui.get_search_text().as_str(), "", "{WHAT}: search cleared");
    assert!(
        labelled(ui, "Search downloads").is_empty(),
        "{WHAT}: search field still there when compact"
    );
    assert_eq!(
        ui.get_adv_rows().row_count() as i32,
        ui.get_tab_counts().waiting,
        "{WHAT}: Waiting rows unfiltered"
    );
    ui.window().set_size(slint::LogicalSize::new(1200.0, 640.0));
    wait_until(&format!("{WHAT}: wide again"), || ui.get_layout() == "wide");
    assert_eq!(field_text(), "", "{WHAT}: field empty");
    assert_eq!(ids(), paused, "{WHAT}: list unfiltered by search");

    // Removing the selected download clears the selection and the pane.
    ctx.rt.block_on(ctx.engine.remove(id, true)).unwrap();
    wait_until(&format!("{WHAT}: selection cleared"), || {
        ui.get_selected_id() == -1 && !ui.get_detail().visible
    });
    assert!(
        labelled(ui, "Select a download to see its details.")
            .iter()
            .any(|e| e.size().width > 0.0),
        "{WHAT}: detail pane not empty"
    );
    ui.window().set_size(COMPACT);
    wait_until(&format!("{WHAT}: compact at the end"), || {
        ui.get_layout() == "compact"
    });

    // Back to Simple, from Advanced Settings' General section; picking a
    // preset there replaces the custom speed.
    ui.window().set_size(SETTINGS_TALL);
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
}

/// Opens the detail of `name` in the compact layout: its tab, then its row.
fn open_detail(ui: &AppWindow, tab: &str, name: &str, what: &str) {
    assert_eq!(ui.get_layout().as_str(), "compact", "{what}: compact");
    ui.set_screen(Screen::Downloads);
    only_with_role(ui, AccessibleRole::Tab, tab, what).invoke_accessible_default_action();
    wait_until(&format!("{what}: {name} listed"), || {
        labelled(ui, name)
            .iter()
            .any(|e| e.accessible_role() == Some(AccessibleRole::ListItem))
    });
    only_with_role(ui, AccessibleRole::ListItem, name, what).invoke_accessible_default_action();
    wait_until(&format!("{what}: detail of {name}"), || {
        let d = ui.get_detail();
        d.visible && d.name == name
    });
}

/// Back closes the compact detail: nothing selected, the list again.
fn close_detail(ui: &AppWindow, what: &str) {
    only_with_role(ui, AccessibleRole::Button, "Back", what).invoke_accessible_default_action();
    assert_eq!(ui.get_selected_id(), -1, "{what}: still selected");
    assert!(!ui.get_detail().visible, "{what}: detail still shown");
    only_with_role(ui, AccessibleRole::Tab, "Active", what);
}

/// The id of the job named `name`.
fn job_named(ctx: &Ctx, name: &str) -> Option<taurus_core::JobView> {
    ctx.engine
        .snapshot()
        .into_iter()
        .find(|v| v.file_name == name)
}

/// Selecting a web download in the compact layout opens its detail over
/// the list: the segment map fills in as it downloads (the detail is
/// fetched again while shown), its Resume and Pause work, and "Copy link"
/// keeps the clipboard watcher from offering the link back.
fn http_detail_shows_cells(ui: &AppWindow, ctx: &Ctx, id: JobId) {
    const WHAT: &str = "http_detail_shows_cells";
    // The detail is fetched again only while the window is shown.
    ui.show().unwrap();
    ui.window().set_size(TALL);
    // The list's own state (here its open link field; also its scroll
    // position) outlives the detail laid over it.
    only_button(ui, "Add a download", WHAT).invoke_accessible_default_action();
    let link_field = || {
        labelled(ui, "Paste a link")
            .iter()
            .any(|e| e.accessible_role() == Some(AccessibleRole::TextInput))
    };
    assert!(link_field(), "{WHAT}: no link field");
    open_detail(ui, "Waiting", "adv.bin", WHAT);
    assert!(!link_field(), "{WHAT}: the list shows under the detail");
    assert_eq!(ui.get_selected_id(), id.0 as i32, "{WHAT}: selected");
    let detail = ui.get_detail();
    assert_eq!(
        (detail.source.as_str(), detail.row.state.as_str()),
        ("HTTP", "paused"),
        "{WHAT}"
    );
    assert!(detail.has_cells, "{WHAT}: no segment map");
    assert_eq!(detail.cells.row_count(), 96, "{WHAT}: cells");
    let map = only_with_role(ui, AccessibleRole::ProgressIndicator, "Segment map", WHAT);
    assert!(map.size().width > 0.0, "{WHAT}: segment map hidden");
    let filled = || ui.get_detail().cells.iter().sum::<f32>();
    let before = filled();

    only_button(ui, "Resume", WHAT).invoke_accessible_default_action();
    wait_until(&format!("{WHAT}: the map fills in"), || {
        ui.get_detail().row.state == "downloading" && filled() > before + 1.0
    });
    only_button(ui, "Pause", WHAT).invoke_accessible_default_action();
    wait_until(&format!("{WHAT}: paused again"), || {
        ui.get_detail().row.state == "paused"
    });

    // Sharing rules the engine refuses (not a torrent) bring the
    // engine's values back.
    let epoch = ui.get_detail().overrides_epoch;
    ui.invoke_set_override(id.0 as i32, "ratio".into(), "2".into());
    wait_until(&format!("{WHAT}: refused rules reset"), || {
        ui.get_detail().overrides_epoch != epoch
    });

    // "Copy link": the watcher does not offer it; other links still are.
    let url = ui.get_detail().url.to_string();
    assert!(url.ends_with("/adv.bin"), "{WHAT}: {url}");
    only_button(ui, "Copy link", WHAT).invoke_accessible_default_action();
    ctx.app.sender().clipboard(url);
    settle(Duration::from_millis(300));
    assert!(!ui.get_banner().visible, "{WHAT}: copied link offered");
    ctx.app
        .sender()
        .clipboard("https://example.org/other.iso".into());
    wait_until(&format!("{WHAT}: other link offered"), || {
        ui.get_banner().visible
    });
    ui.invoke_banner_dismiss();

    close_detail(ui, WHAT);
    assert!(
        link_field(),
        "{WHAT}: the list was rebuilt under the detail"
    );
    only_button(ui, "Close the link field", WHAT).invoke_accessible_default_action();
    ui.window().set_size(COMPACT);
}

/// Files in the output folder whose names start with `prefix` (a
/// download's file and its partial file).
fn files_starting(ctx: &Ctx, prefix: &str) -> Vec<String> {
    std::fs::read_dir(ctx.out())
        .unwrap()
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter(|n| n.starts_with(prefix))
        .collect()
}

/// The Remove dialog (ruling R10) keeps a finished download's file unless
/// "Also delete the files" is ticked, which it is not when the dialog
/// opens; an unfinished download's partial file goes either way.
fn remove_dialog_defaults_to_keep(ui: &AppWindow, ctx: &Ctx) {
    const WHAT: &str = "remove_dialog_defaults_to_keep";
    ui.window().set_size(TALL);

    // Unfinished, paused part-way: removed unticked, its partial file goes.
    let mut spec = FileSpec::new(20 * 1024 * 1024);
    spec.bytes_per_sec = Some(256 * 1024);
    let url = ctx.server.serve("part.bin", spec);
    let request = AddRequest {
        url,
        dest_dir: ctx.out(),
        ..Default::default()
    };
    let id = ctx.rt.block_on(ctx.engine.add(request)).unwrap();
    wait_until(&format!("{WHAT}: part.bin under way"), || {
        job_named(ctx, "part.bin")
            .is_some_and(|v| v.state == JobState::Downloading && v.downloaded > 0)
    });
    ctx.rt.block_on(ctx.engine.pause(id)).unwrap();
    wait_until(&format!("{WHAT}: part.bin paused"), || {
        job_named(ctx, "part.bin").is_some_and(|v| v.state == JobState::Paused)
    });
    assert!(
        !files_starting(ctx, "part.bin").is_empty(),
        "{WHAT}: no partial file"
    );
    open_detail(ui, "Waiting", "part.bin", WHAT);
    only_button(ui, "Remove…", WHAT).invoke_accessible_default_action();
    let delete = only_with_role(ui, AccessibleRole::Checkbox, "Also delete the files", WHAT);
    assert_eq!(delete.accessible_checked(), Some(false), "{WHAT}: ticked");
    only_button(ui, "Remove", WHAT).invoke_accessible_default_action();
    wait_until(&format!("{WHAT}: part.bin removed"), || {
        job_named(ctx, "part.bin").is_none()
    });
    wait_for(
        Duration::from_secs(10),
        &format!("{WHAT}: partial file deleted"),
        || files_starting(ctx, "part.bin").is_empty(),
    );
    wait_until(&format!("{WHAT}: detail closed after part.bin"), || {
        !ui.get_detail().visible
    });

    // Finished: kept unticked, deleted ticked.
    for (name, tick) in [("keep.bin", false), ("gone.bin", true)] {
        let url = ctx.server.serve(name, FileSpec::new(64 * 1024));
        let request = AddRequest {
            url,
            dest_dir: ctx.out(),
            ..Default::default()
        };
        let id = ctx.rt.block_on(ctx.engine.add(request)).unwrap();
        wait_until(&format!("{WHAT}: {name} finished"), || {
            job_named(ctx, name).is_some_and(|v| v.state == JobState::Completed)
        });
        let file = ctx.out().join(name);
        assert!(file.is_file(), "{WHAT}: {name} not saved");
        open_detail(ui, "Done", name, WHAT);
        only_button(ui, "Remove…", WHAT).invoke_accessible_default_action();
        let dialog = ui.get_remove_dialog();
        assert_eq!(
            (dialog.visible, dialog.id, dialog.name.as_str()),
            (true, id.0 as i32, name),
            "{WHAT}: dialog"
        );
        let question = format!("Remove {name} from the list?");
        assert!(
            labelled(ui, &question).iter().any(|e| e.size().width > 0.0),
            "{WHAT}: no {question:?}"
        );
        let delete = only_with_role(ui, AccessibleRole::Checkbox, "Also delete the files", WHAT);
        assert_eq!(
            delete.accessible_checked(),
            Some(false),
            "{WHAT}: ticked when opened"
        );
        if tick {
            delete.invoke_accessible_default_action();
            assert_eq!(delete.accessible_checked(), Some(true), "{WHAT}: tick");
        }
        only_button(ui, "Remove", WHAT).invoke_accessible_default_action();
        assert!(!ui.get_remove_dialog().visible, "{WHAT}: dialog stays");
        wait_until(&format!("{WHAT}: {name} removed"), || {
            job_named(ctx, name).is_none()
        });
        wait_until(&format!("{WHAT}: detail closed after {name}"), || {
            ui.get_selected_id() == -1 && !ui.get_detail().visible
        });
        assert_eq!(file.is_file(), !tick, "{WHAT}: {name} on disk");
    }
    // Cancel changes nothing.
    let url = ctx.server.serve("stay.bin", FileSpec::new(1024));
    let request = AddRequest {
        url,
        dest_dir: ctx.out(),
        ..Default::default()
    };
    ctx.rt.block_on(ctx.engine.add(request)).unwrap();
    wait_until(&format!("{WHAT}: stay.bin finished"), || {
        job_named(ctx, "stay.bin").is_some_and(|v| v.state == JobState::Completed)
    });
    open_detail(ui, "Done", "stay.bin", WHAT);
    only_button(ui, "Remove…", WHAT).invoke_accessible_default_action();
    // Unticked again after the ticked removal above.
    let delete = only_with_role(ui, AccessibleRole::Checkbox, "Also delete the files", WHAT);
    assert_eq!(
        delete.accessible_checked(),
        Some(false),
        "{WHAT}: still ticked"
    );
    // The repeats of an Enter held since before the dialog opened, then
    // Return on the box: nothing is removed, the dialog stays.
    for _ in 0..3 {
        ui.window().dispatch_event(WindowEvent::KeyPressRepeated {
            text: slint::platform::Key::Return.into(),
        });
    }
    press(ui, slint::platform::Key::Tab);
    press(ui, slint::platform::Key::Return);
    settle(Duration::from_millis(300));
    assert!(
        ui.get_remove_dialog().visible,
        "{WHAT}: dialog closed by keys"
    );
    assert!(
        job_named(ctx, "stay.bin").is_some(),
        "{WHAT}: removed by a held Enter or Return on the box"
    );
    let delete = only_with_role(ui, AccessibleRole::Checkbox, "Also delete the files", WHAT);
    assert_eq!(delete.accessible_checked(), Some(true), "{WHAT}: Return");
    only_button(ui, "Cancel", WHAT).invoke_accessible_default_action();
    assert!(!ui.get_remove_dialog().visible, "{WHAT}: cancel");
    settle(Duration::from_millis(300));
    assert!(
        job_named(ctx, "stay.bin").is_some(),
        "{WHAT}: cancel removed"
    );
    close_detail(ui, WHAT);
    ui.window().set_size(COMPACT);
}

/// A protected download that failed for want of a password asks for it
/// when its detail opens; submitting hands it to the engine and empties
/// both fields at once, and the download then finishes.
fn password_cleared_after_submit(ui: &AppWindow, ctx: &Ctx) {
    const WHAT: &str = "password_cleared_after_submit";
    const USER: &str = "hyades-user";
    const PASSWORD: &str = "s3cr3t-Pa55";
    const SIZE: usize = 512 * 1024;
    // The probe when adding succeeds; the download is then refused (401):
    // the job fails with NeedsSignIn. Then the file wants the password.
    let mut spec = FileSpec::new(SIZE);
    spec.fail_after = Some(FailAfter {
        after_requests: 1,
        status: 401,
        headers: vec![("WWW-Authenticate".into(), "Basic realm=\"test\"".into())],
    });
    let url = ctx.server.serve("locked.bin", spec);
    let request = AddRequest {
        url,
        dest_dir: ctx.out(),
        ..Default::default()
    };
    ctx.rt.block_on(ctx.engine.add(request)).unwrap();
    wait_until(&format!("{WHAT}: locked.bin needs a password"), || {
        job_named(ctx, "locked.bin").is_some_and(|v| {
            v.state == JobState::Failed
                && v.error
                    .as_ref()
                    .is_some_and(|(k, _)| *k == ErrorKind::NeedsSignIn)
        })
    });
    let mut protected = FileSpec::new(SIZE);
    protected.basic_auth = Some((USER.into(), PASSWORD.into()));
    ctx.server.replace("locked.bin", protected);

    ui.window().set_size(TALL);
    open_detail(ui, "Waiting", "locked.bin", WHAT);
    // The window learns of the failure on a later tick than the engine.
    wait_until(&format!("{WHAT}: needs-password"), || {
        ui.get_detail().needs_password
    });
    wait_until(&format!("{WHAT}: dialog shown"), || {
        ui.get_password_dialog().visible
    });
    assert_eq!(ui.get_password_dialog().name.as_str(), "locked.bin");
    let field = |label: &str| only_with_role(ui, AccessibleRole::TextInput, label, WHAT);
    let value = |label: &str| {
        field(label)
            .accessible_value()
            .unwrap_or_default()
            .to_string()
    };
    let empty = |when: &str| {
        assert_eq!(
            ui.get_password_text().as_str(),
            "",
            "{WHAT}: {when}: password kept"
        );
        assert_eq!(
            ui.get_password_user().as_str(),
            "",
            "{WHAT}: {when}: user kept"
        );
    };

    // Cancel empties both fields; "Enter password…" asks again, empty.
    field("User name").set_accessible_value(USER);
    field("Password").set_accessible_value("typed-then-cancelled");
    only_button(ui, "Cancel", WHAT).invoke_accessible_default_action();
    assert!(!ui.get_password_dialog().visible, "{WHAT}: cancel");
    empty("after Cancel");
    settle(Duration::from_millis(700));
    assert!(
        !ui.get_password_dialog().visible,
        "{WHAT}: asked again by itself"
    );
    only_button(ui, "Enter password…", WHAT).invoke_accessible_default_action();
    assert!(ui.get_password_dialog().visible, "{WHAT}: Enter password…");
    assert_eq!(
        (value("User name"), value("Password")),
        (String::new(), String::new())
    );

    // A wrong password: emptied at once, the dialog waits, then says so.
    let submit = |password: &str| {
        field("User name").set_accessible_value(USER);
        field("Password").set_accessible_value(password);
        assert_eq!(ui.get_password_text().as_str(), password, "{WHAT}: typed");
        only_button(ui, "Sign in", WHAT).invoke_accessible_default_action();
        // Emptied as soon as they are handed over, while the dialog waits
        // (the engine's answer arrives on a later tick).
        empty("after Sign in");
        let dialog = ui.get_password_dialog();
        assert!(dialog.visible && dialog.busy, "{WHAT}: {dialog:?}");
        assert_eq!(value("Password"), "", "{WHAT}: password field not empty");
        assert_eq!(value("User name"), "", "{WHAT}: user field not empty");
    };
    submit("not-it");
    wait_until(&format!("{WHAT}: refused"), || {
        let d = ui.get_password_dialog();
        d.visible && !d.busy && d.wrong
    });
    empty("after the refusal");
    assert!(
        !labelled(ui, "That user name or password didn't work. Try again.").is_empty(),
        "{WHAT}: refusal not shown"
    );
    // The right one.
    submit(PASSWORD);
    wait_until(&format!("{WHAT}: dialog closed"), || {
        !ui.get_password_dialog().visible
    });
    wait_until(&format!("{WHAT}: locked.bin finished"), || {
        job_named(ctx, "locked.bin").is_some_and(|v| v.state == JobState::Completed)
    });
    let file = ctx.out().join("locked.bin");
    assert_eq!(
        std::fs::metadata(&file).map(|m| m.len()).ok(),
        Some(SIZE as u64),
        "{WHAT}: not saved in full"
    );
    // The detail follows: it no longer asks for a password.
    wait_until(&format!("{WHAT}: detail done"), || {
        let d = ui.get_detail();
        d.row.state == "done" && !d.needs_password
    });
    assert!(!ui.get_password_dialog().visible, "{WHAT}: asked again");
    close_detail(ui, WHAT);
    ui.window().set_size(COMPACT);
}

/// A torrent's detail lists its files; the file picker's choice
/// (`files-chosen`) and the sharing selects reach the engine.
fn torrent_detail_files_and_overrides() {
    const WHAT: &str = "torrent_detail_files_and_overrides";
    let rt = runtime();
    let data = |n| taurus_testkit::content(&FileSpec::new(n));
    let seeder = rt.block_on(TorrentSeeder::start(
        "pack",
        &[("a.bin", data(128 * 1024)), ("b.bin", data(64 * 1024))],
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
        settings.ui.mode = UiMode::Advanced;
    });
    ui.show().unwrap();
    ui.window().set_size(TALL);
    ui.set_screen(Screen::Downloads);
    wait_until(&format!("{WHAT}: compact"), || ui.get_layout() == "compact");
    let request = AddRequest {
        url: seeder.magnet.clone(),
        dest_dir: ctx.out(),
        ..Default::default()
    };
    let id = ctx.rt.block_on(ctx.engine.add(request)).unwrap();
    let view = || {
        ctx.engine
            .snapshot()
            .into_iter()
            .find(|v| v.id == id)
            .unwrap()
    };
    wait_for(Duration::from_secs(60), &format!("{WHAT}: sharing"), || {
        view().state == JobState::Seeding
    });
    open_detail(&ui, "Active", "pack", WHAT);
    wait_until(&format!("{WHAT}: two files"), || {
        let d = ui.get_detail();
        d.files.row_count() == 2 && d.files.iter().all(|f| f.selected && f.progress == 1.0)
    });
    let d = ui.get_detail();
    assert!(d.can_choose_files && !d.has_cells, "{WHAT}: {d:?}");
    // Torrent index order, whatever it is: the seeder's torrent lists the
    // files in directory-listing order, which differs between file systems
    // (APFS lists b.bin first).
    let engine_files = {
        let _rt = ctx.rt.enter();
        ctx.engine.job_detail(id).unwrap().files
    };
    let mut paths: Vec<&str> = engine_files.iter().map(|f| f.path.as_str()).collect();
    paths.sort_unstable();
    assert_eq!(paths, ["a.bin", "b.bin"], "{WHAT}: {engine_files:?}");
    let rows: Vec<(i32, String)> = d
        .files
        .iter()
        .map(|f| (f.index, f.name.to_string()))
        .collect();
    let by_index: Vec<(i32, String)> = (0..engine_files.len())
        .map(|i| {
            let f = engine_files.iter().find(|f| f.index == i).unwrap();
            (i as i32, f.path.clone())
        })
        .collect();
    assert_eq!(rows, by_index, "{WHAT}: rows not in index order");
    only_button(&ui, "Choose files…", WHAT);

    // The picker's answer: only b.bin.
    let want = id.0 as i32;
    let b = by_index.iter().find(|(_, p)| p == "b.bin").unwrap().0;
    ui.invoke_files_chosen(want, ModelRc::new(VecModel::from(vec![b])));
    wait_until(&format!("{WHAT}: one file chosen"), || {
        view().torrent.is_some_and(|t| t.files_selected == 1)
    });
    let only_b: Vec<bool> = by_index.iter().map(|(_, p)| p == "b.bin").collect();
    wait_until(&format!("{WHAT}: the detail shows it"), || {
        let files: Vec<bool> = ui.get_detail().files.iter().map(|f| f.selected).collect();
        files == only_b
    });
    let chosen = {
        let _rt = ctx.rt.enter();
        ctx.engine.job_detail(id).unwrap().files
    };
    assert!(
        chosen.iter().all(|f| f.selected == (f.path == "b.bin")),
        "{WHAT}: {chosen:?}"
    );

    // Sharing rules: each select keeps the other's value.
    assert_eq!(
        (
            ui.get_detail().ratio_override.as_str(),
            ui.get_detail().seed_time_override.as_str()
        ),
        ("default", "default"),
        "{WHAT}"
    );
    ui.invoke_set_override(want, "ratio".into(), "2".into());
    ui.invoke_set_override(want, "seed-time".into(), "3600".into());
    wait_until(&format!("{WHAT}: overrides saved"), || {
        let o = view().torrent.unwrap().overrides;
        o.stop_ratio == Some(Some(2.0)) && o.stop_after_secs == Some(Some(3600))
    });
    wait_until(&format!("{WHAT}: overrides shown"), || {
        let d = ui.get_detail();
        d.ratio_override == "2" && d.seed_time_override == "3600"
    });
    ui.invoke_set_override(want, "ratio".into(), "default".into());
    wait_until(&format!("{WHAT}: ratio back to the setting"), || {
        let o = view().torrent.unwrap().overrides;
        o.stop_ratio.is_none() && o.stop_after_secs == Some(Some(3600))
    });
    ctx.shutdown(WHAT);
    drop(seeder);
}

/// Converting a finished song from its Advanced detail (the fake ffmpeg)
/// adds a Convert row whose progress is media time.
fn convert_shows_media_time(m: &MediaCtx) {
    const WHAT: &str = "convert_shows_media_time";
    const SONG: &str = "A Fine Video [abc123].mp3";
    let ui = &m.ui;
    // Slow enough that the row is seen under way. Only this test runs in
    // the process, and nothing else reads the environment meanwhile.
    unsafe { std::env::set_var("FAKE_FFMPEG_STEP_MS", "500") };
    ui.show().unwrap();
    ui.window().set_size(TALL);
    ui.invoke_setting_changed("advanced".into(), "true".into());
    open_detail(ui, "Done", SONG, WHAT);
    assert!(ui.get_detail().can_convert, "{WHAT}: cannot convert");
    only_button(ui, "Convert…", WHAT).invoke_accessible_default_action();
    assert!(
        labelled(ui, "Make a smaller MP4").is_empty(),
        "{WHAT}: a song offered as a video"
    );
    only_button(ui, "Convert to MP3", WHAT).invoke_accessible_default_action();
    close_detail(ui, WHAT);
    let converting = || {
        ui.get_adv_rows()
            .iter()
            .find(|r| r.source == "Convert" && r.row.state == "downloading")
    };
    wait_for(TOOLS_WAIT, &format!("{WHAT}: converting"), || {
        only_with_role(ui, AccessibleRole::Tab, "Active", WHAT).invoke_accessible_default_action();
        converting().is_some_and(|r| r.row.done_text != "0:00")
    });
    let row = converting().unwrap().row;
    assert_eq!(row.total_text.as_str(), "0:03", "{WHAT}: {row:?}");
    let status = format!("{} of 0:03", row.done_text);
    assert!(
        !texts_containing(ui, &status).is_empty(),
        "{WHAT}: no {status:?} shown"
    );
    let out = m.music().join("A Fine Video [abc123] (mp3).mp3");
    wait_for(TOOLS_WAIT, &format!("{WHAT}: converted"), || {
        out.is_file()
            && m.ctx
                .engine
                .snapshot()
                .iter()
                .any(|v| v.kind == taurus_core::JobKind::Convert && v.state == JobState::Completed)
    });
    unsafe { std::env::remove_var("FAKE_FFMPEG_STEP_MS") };
    ui.invoke_setting_changed("advanced".into(), "false".into());
}

/// Another link to the fake's video (the first one is in the list).
const ADV_VIDEO_LINK: &str = "https://www.youtube.com/watch?v=hyadesadv";

/// Advanced add of a video: every height the video has is offered, and
/// the quality, format, subtitles and browser picked reach yt-dlp.
fn adv_add_media_options(m: &MediaCtx) {
    const WHAT: &str = "adv_add_media_options";
    let ui = &m.ui;
    ui.window().set_size(TALL);
    ui.invoke_setting_changed("advanced".into(), "true".into());
    assert_ne!(ui.get_layout().as_str(), "simple", "{WHAT}: Advanced");
    ui.invoke_link_entered(ADV_VIDEO_LINK.into());
    assert_eq!(ui.get_screen(), Screen::Add, "{WHAT}: not on Add");
    wait_for(TOOLS_WAIT, &format!("{WHAT}: shows the video"), || {
        let a = ui.get_adv_add();
        !a.loading && a.kind == "media"
    });
    let a = ui.get_adv_add();
    assert_eq!(a.error_kind.as_str(), "", "{WHAT}: {a:?}");
    assert_eq!(
        (
            a.title.as_str(),
            a.uploader.as_str(),
            a.duration_text.as_str(),
            a.quality_id.as_str(),
            a.container.as_str(),
        ),
        ("A Fine Video", "Some Channel", "3:32", "best", "mp4"),
        "{WHAT}: {a:?}"
    );
    // Every height of the fixture's formats, highest first.
    let heights: Vec<i32> = a
        .qualities
        .iter()
        .filter(|q| q.height > 0)
        .map(|q| q.height)
        .collect();
    assert_eq!(heights, [1080, 720], "{WHAT}: {:?}", a.qualities);
    let ids: Vec<String> = a.qualities.iter().map(|q| q.id.to_string()).collect();
    assert!(
        ids.iter().any(|id| id == "1080p") && ids.iter().any(|id| id == "720p"),
        "{WHAT}: {ids:?}"
    );
    assert!(ids.iter().any(|id| id == "audio-best"), "{WHAT}: {ids:?}");
    let langs: Vec<String> = a
        .subtitle_langs
        .iter()
        .map(|l| l.code.to_string())
        .collect();
    assert_eq!(langs, ["en", "vi"], "{WHAT}: subtitle languages");
    // The Advanced screen, not Simple's.
    only_with_role(ui, AccessibleRole::Checkbox, "Start immediately", WHAT);
    assert!(
        labelled(ui, "Other quality").is_empty(),
        "{WHAT}: Simple's Add screen shown"
    );

    for (field, value) in [
        ("quality", "720p"),
        ("container", "mkv"),
        ("sub-lang", "en"),
        ("embed-subs", "true"),
        ("cookies-browser", "firefox"),
    ] {
        ui.invoke_adv_field_changed(field.into(), value.into());
    }
    let a = ui.get_adv_add();
    assert_eq!(
        (
            a.quality_id.as_str(),
            a.container.as_str(),
            a.embed_subs,
            a.cookies_browser.as_str(),
        ),
        ("720p", "mkv", true, "firefox"),
        "{WHAT}: {a:?}"
    );
    let picked: Vec<String> = a
        .subtitle_langs
        .iter()
        .filter(|l| l.selected)
        .map(|l| l.code.to_string())
        .collect();
    assert_eq!(picked, ["en"], "{WHAT}: picked languages");
    let p720 = only_with_role(ui, AccessibleRole::RadioButton, "720p", WHAT);
    assert_eq!(p720.accessible_checked(), Some(true), "{WHAT}: 720p shown");

    ui.invoke_adv_add_confirmed(true);
    wait_until(&format!("{WHAT}: on the list"), || {
        ui.get_screen() == Screen::Downloads
    });
    let run = || {
        process_args_log()
            .into_iter()
            .filter(|e| e.tool == "yt-dlp" && !e.argv.iter().any(|a| a == "-J"))
            .map(|e| e.argv)
            .find(|argv| {
                argv.windows(2)
                    .any(|w| w == ["--merge-output-format", "mkv"])
            })
    };
    wait_for(TOOLS_WAIT, &format!("{WHAT}: yt-dlp ran"), || {
        run().is_some()
    });
    let argv = run().unwrap();
    let pair = |a: &str, b: &str| argv.windows(2).any(|w| w[0] == a && w[1] == b);
    assert!(
        argv.windows(2)
            .any(|w| w[0] == "-f" && w[1].starts_with("bv*[height<=720]")),
        "{WHAT}: {argv:?}"
    );
    assert!(pair("--sub-langs", "en"), "{WHAT}: {argv:?}");
    assert!(argv.iter().any(|a| a == "--embed-subs"), "{WHAT}: {argv:?}");
    assert!(
        pair("--cookies-from-browser", "firefox"),
        "{WHAT}: {argv:?}"
    );
    wait_for(TOOLS_WAIT, &format!("{WHAT}: finished"), || {
        m.ctx
            .engine
            .snapshot()
            .iter()
            .any(|v| v.source_url == ADV_VIDEO_LINK && v.state == JobState::Completed)
    });
    ui.invoke_setting_changed("advanced".into(), "false".into());
}

/// Advanced add of a file link: its name, connections, mirror, checksum
/// and sign-in reach the engine; the password is emptied as it is handed
/// over and is never in the view model.
fn adv_add_file_mirrors_checksum(ui: &AppWindow, ctx: &Ctx) {
    const WHAT: &str = "adv_add_file_mirrors_checksum";
    const PASSWORD: &str = "hunter2-typed";
    let spec = FileSpec::new(1024 * 1024);
    let url = ctx.server.serve("mirrored.bin", spec.clone());
    let mirror = ctx.rt.block_on(TestServer::start());
    let mirror_url = mirror.serve("mirrored.bin", spec.clone());
    let sum = {
        use sha2::Digest;
        hex::encode(sha2::Sha256::digest(taurus_testkit::content(&spec)))
    };
    ui.window().set_size(TALL);
    ui.set_screen(Screen::Downloads);
    ui.invoke_setting_changed("advanced".into(), "true".into());
    assert_eq!(ui.get_layout().as_str(), "compact", "{WHAT}: Advanced");
    ui.invoke_link_entered(url.as_str().into());
    assert_eq!(ui.get_screen(), Screen::Add, "{WHAT}: not on Add");
    wait_until(&format!("{WHAT}: shows the file"), || {
        let a = ui.get_adv_add();
        !a.loading && a.kind == "file"
    });
    let a = ui.get_adv_add();
    assert_eq!(
        (
            a.name.as_str(),
            a.size_text.as_str(),
            a.connections,
            a.checksum_ok,
            a.error_kind.as_str()
        ),
        ("mirrored.bin", "1.0 MB", 16, true, ""),
        "{WHAT}: {a:?}"
    );
    for label in ["File name", "Mirrors", "Checksum", "Referer", "Password"] {
        only_with_role(ui, AccessibleRole::TextInput, label, WHAT);
    }
    only_with_role(ui, AccessibleRole::Checkbox, "Start immediately", WHAT);

    ui.invoke_adv_field_changed("checksum".into(), "sha256:not-a-sum".into());
    assert!(!ui.get_adv_add().checksum_ok, "{WHAT}: bad checksum taken");
    for (field, value) in [
        ("mirrors", format!("{mirror_url}\n\n")),
        ("checksum", format!("sha256:{sum}")),
        ("connections", "4".to_string()),
        ("name", "copy.bin".to_string()),
        ("user", "someone".to_string()),
    ] {
        ui.invoke_adv_field_changed(field.into(), value.into());
    }
    let a = ui.get_adv_add();
    assert!(a.checksum_ok, "{WHAT}: {a:?}");
    assert_eq!(a.connections, 4, "{WHAT}");
    let mirrors: Vec<String> = a.mirrors.iter().map(|m| m.to_string()).collect();
    assert_eq!(
        mirrors,
        std::slice::from_ref(&mirror_url),
        "{WHAT}: mirrors"
    );
    ui.set_adv_password(PASSWORD.into());
    // "Start immediately" unticked: added and left to the queue.
    ui.invoke_adv_add_confirmed(false);
    assert_eq!(ui.get_adv_password().as_str(), "", "{WHAT}: password kept");
    assert!(
        !format!("{:?}", ui.get_adv_add()).contains(PASSWORD),
        "{WHAT}: password in the view model"
    );
    wait_until(&format!("{WHAT}: on the list"), || {
        ui.get_screen() == Screen::Downloads
    });
    wait_until(&format!("{WHAT}: copy.bin finished"), || {
        job_named(ctx, "copy.bin").is_some_and(|v| v.state == JobState::Completed)
    });
    let id = job_named(ctx, "copy.bin").unwrap().id;
    let detail = {
        let _rt = ctx.rt.enter();
        ctx.engine.job_detail(id).unwrap()
    };
    assert_eq!(detail.mirrors, [mirror_url], "{WHAT}: mirrors");
    assert_eq!(
        detail.checksum.as_deref(),
        Some(format!("sha256:{sum}").as_str()),
        "{WHAT}: checksum"
    );
    assert!(detail.has_password, "{WHAT}: no password kept");
    let file = ctx.documents().join("copy.bin");
    assert_eq!(
        std::fs::metadata(&file).map(|m| m.len()).ok(),
        Some(1024 * 1024),
        "{WHAT}: {} not saved in full",
        file.display()
    );
    ui.invoke_setting_changed("advanced".into(), "false".into());
    ui.window().set_size(COMPACT);
}

/// The Advanced add's password: emptied by Escape, by the screen going
/// (a mode switch, or any other screen), not by a Download that cannot go
/// yet; after a refused add the field says to type it again and an empty
/// one is not sent.
fn adv_add_password_never_lingers(ui: &AppWindow, ctx: &Ctx) {
    use slint::platform::Key;
    const WHAT: &str = "adv_add_password_never_lingers";
    const USER: &str = "lingers-user";
    const PASSWORD: &str = "l1ngers-Pa55";
    let mut spec = FileSpec::new(64 * 1024);
    spec.basic_auth = Some((USER.into(), PASSWORD.into()));
    let url = ctx.server.serve("guarded.bin", spec);
    ui.window().set_size(TALL);
    ui.set_screen(Screen::Downloads);
    ui.invoke_setting_changed("advanced".into(), "true".into());
    let open = || {
        ui.invoke_link_entered(url.as_str().into());
        wait_until(&format!("{WHAT}: asks to sign in"), || {
            let a = ui.get_adv_add();
            !a.loading && a.kind == "file" && a.needs_user
        });
        refresh_tree(ui);
    };
    let field = |label: &str| only_with_role(ui, AccessibleRole::TextInput, label, WHAT);
    let typed = || ui.get_adv_password().to_string();

    open();
    field("Password").set_accessible_value("typed-then-escaped");
    assert_eq!(typed(), "typed-then-escaped", "{WHAT}: typed");
    press(ui, Key::Escape);
    assert_eq!(typed(), "", "{WHAT}: Escape kept it");

    // Not ready (a bad checksum): Download keeps what is typed.
    open();
    field("Password").set_accessible_value(PASSWORD);
    ui.invoke_adv_field_changed("checksum".into(), "not-a-sum".into());
    ui.invoke_adv_add_confirmed(false);
    assert_eq!(typed(), PASSWORD, "{WHAT}: emptied before it could go");
    ui.invoke_adv_field_changed("checksum".into(), "".into());
    // Another screen, however it came: gone.
    ui.set_screen(Screen::Downloads);
    wait_until(&format!("{WHAT}: kept after the screen went"), || {
        typed().is_empty()
    });
    ui.invoke_add_cancelled();

    // The mode switch takes the screen away, and the password with it.
    open();
    field("Password").set_accessible_value(PASSWORD);
    ui.invoke_setting_changed("advanced".into(), "false".into());
    assert_eq!(ui.get_layout().as_str(), "simple", "{WHAT}: Simple");
    assert_eq!(typed(), "", "{WHAT}: kept after the mode switch");
    ui.invoke_add_cancelled();
    ui.set_screen(Screen::Downloads);
    ui.invoke_setting_changed("advanced".into(), "true".into());

    // A wrong password: refused, the field empty and asking again.
    open();
    ui.invoke_adv_field_changed("user".into(), USER.into());
    field("Password").set_accessible_value("not-it");
    ui.invoke_adv_add_confirmed(true);
    assert_eq!(typed(), "", "{WHAT}: kept after Download");
    wait_until(&format!("{WHAT}: refused"), || {
        let a = ui.get_adv_add();
        !a.busy && !a.refused.is_empty()
    });
    assert!(ui.get_adv_add().password_again, "{WHAT}: not asked again");
    refresh_tree(ui);
    assert_eq!(
        field("Password")
            .accessible_placeholder_text()
            .unwrap_or_default()
            .as_str(),
        "Type the password again",
        "{WHAT}: the field does not ask again"
    );
    // Download with the field empty sends nothing.
    ui.invoke_adv_add_confirmed(true);
    assert!(!ui.get_adv_add().busy, "{WHAT}: an empty password sent");
    field("Password").set_accessible_value(PASSWORD);
    ui.invoke_adv_add_confirmed(true);
    wait_until(&format!("{WHAT}: on the list"), || {
        ui.get_screen() == Screen::Downloads
    });
    wait_until(&format!("{WHAT}: guarded.bin finished"), || {
        job_named(ctx, "guarded.bin").is_some_and(|v| v.state == JobState::Completed)
    });
    ui.invoke_setting_changed("advanced".into(), "false".into());
    ui.window().set_size(COMPACT);
}

/// The picker rows' (path, group, state) and the index of a file by path.
fn picker_rows(files: &ModelRc<taurus_desktop::TorrentFileVm>) -> Vec<(String, bool, String)> {
    files
        .iter()
        .map(|r| {
            let state = if r.group {
                r.state.to_string()
            } else if r.selected {
                "all".to_string()
            } else {
                "none".to_string()
            };
            (r.path.to_string(), r.group, state)
        })
        .collect()
}

fn picker_index(files: &ModelRc<taurus_desktop::TorrentFileVm>, path: &str) -> i32 {
    files
        .iter()
        .find(|r| !r.group && r.path == path)
        .unwrap_or_else(|| panic!("no file {path}"))
        .index
}

/// Advanced add of a torrent: files grouped by folder with tri-state group
/// boxes; the selection and the sharing rule reach the engine; "Choose
/// files…" of the job opens the same picker, prefilled.
fn adv_add_torrent_picker() {
    const WHAT: &str = "adv_add_torrent_picker";
    let rt = runtime();
    let data = |n| taurus_testkit::content(&FileSpec::new(n));
    let seeder = rt.block_on(TorrentSeeder::start(
        "grouped",
        &[
            ("a/1.bin", data(64 * 1024)),
            ("a/2.bin", data(48 * 1024)),
            ("b.bin", data(32 * 1024)),
        ],
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
        settings.ui.mode = UiMode::Advanced;
    });
    ui.show().unwrap();
    ui.window().set_size(TALL);
    ui.set_screen(Screen::Downloads);
    wait_until(&format!("{WHAT}: compact"), || ui.get_layout() == "compact");
    ui.invoke_link_entered(seeder.magnet.as_str().into());
    assert_eq!(ui.get_screen(), Screen::Add, "{WHAT}: not on Add");
    wait_for(
        Duration::from_secs(60),
        &format!("{WHAT}: shows the torrent"),
        || {
            let a = ui.get_adv_add();
            !a.loading && a.kind == "torrent"
        },
    );
    let a = ui.get_adv_add();
    assert_eq!(a.error_kind.as_str(), "", "{WHAT}: {a:?}");
    let row = |p: &str, g: bool, s: &str| (p.to_string(), g, s.to_string());
    assert_eq!(
        picker_rows(&a.files),
        [
            row("a", true, "all"),
            row("a/1.bin", false, "all"),
            row("a/2.bin", false, "all"),
            row("b.bin", false, "all"),
        ],
        "{WHAT}: grouped rows"
    );
    assert_eq!(a.selected_count, 3, "{WHAT}");
    assert!(!a.free_text.is_empty(), "{WHAT}: free space unknown");
    let one = picker_index(&a.files, "a/1.bin");
    let b = picker_index(&a.files, "b.bin");

    ui.invoke_adv_toggle_group("a".into());
    let a = ui.get_adv_add();
    assert_eq!(
        picker_rows(&a.files),
        [
            row("a", true, "none"),
            row("a/1.bin", false, "none"),
            row("a/2.bin", false, "none"),
            row("b.bin", false, "all"),
        ],
        "{WHAT}: group a off"
    );
    assert_eq!(a.selected_count, 1, "{WHAT}");
    // One file of the group again: the group shows a dash.
    ui.invoke_adv_toggle_file(one);
    let a = ui.get_adv_add();
    assert_eq!(picker_rows(&a.files)[0], row("a", true, "some"), "{WHAT}");
    let group = only_with_role(&ui, AccessibleRole::Checkbox, "a", WHAT);
    assert_eq!(
        group.accessible_value().map(|v| v.to_string()).as_deref(),
        Some("some"),
        "{WHAT}: dash not shown"
    );
    assert_eq!(group.accessible_checked(), Some(false), "{WHAT}");
    ui.invoke_adv_toggle_file(one);
    assert_eq!(ui.get_adv_add().selected_count, 1, "{WHAT}");
    ui.invoke_adv_field_changed("ratio".into(), "2".into());
    assert_eq!(ui.get_adv_add().ratio.as_str(), "2", "{WHAT}");

    ui.invoke_adv_add_confirmed(true);
    wait_until(&format!("{WHAT}: on the list"), || {
        ui.get_screen() == Screen::Downloads
    });
    let view = || {
        ctx.engine
            .snapshot()
            .into_iter()
            .find(|v| v.kind == taurus_core::JobKind::Torrent)
    };
    let id = view().expect("torrent added").id;
    let files = || {
        let _rt = ctx.rt.enter();
        ctx.engine.job_detail(id).unwrap().files
    };
    wait_for(
        Duration::from_secs(60),
        &format!("{WHAT}: metadata"),
        || files().len() == 3,
    );
    // Ruling R11: only_files is [the index of b.bin], whatever it is.
    let engine_files = files();
    let b_index = engine_files
        .iter()
        .find(|f| f.path == "b.bin")
        .map(|f| f.index as i32);
    assert_eq!(b_index, Some(b), "{WHAT}: picker index of b.bin");
    let chosen: Vec<usize> = engine_files
        .iter()
        .filter(|f| f.selected)
        .map(|f| f.index)
        .collect();
    assert_eq!(chosen, [b as usize], "{WHAT}: {engine_files:?}");
    let overrides = view().unwrap().torrent.unwrap().overrides;
    assert_eq!(overrides.stop_ratio, Some(Some(2.0)), "{WHAT}: ratio");

    // "Choose files…": the same picker, prefilled from the job.
    let want = id.0 as i32;
    ui.invoke_choose_files(want);
    let d = ui.get_files_dialog();
    assert!(d.visible && d.id == want, "{WHAT}: {d:?}");
    assert_eq!(
        picker_rows(&d.files),
        [
            row("a", true, "none"),
            row("a/1.bin", false, "none"),
            row("a/2.bin", false, "none"),
            row("b.bin", false, "all"),
        ],
        "{WHAT}: prefilled"
    );
    ui.invoke_adv_toggle_group("a".into());
    let d = ui.get_files_dialog();
    assert_eq!(picker_rows(&d.files)[0], row("a", true, "all"), "{WHAT}");
    assert_eq!(d.chosen.row_count(), 3, "{WHAT}: {d:?}");
    only_button(&ui, "Use these files", WHAT).invoke_accessible_default_action();
    assert!(!ui.get_files_dialog().visible, "{WHAT}: dialog open");
    wait_until(&format!("{WHAT}: all files chosen"), || {
        view()
            .and_then(|v| v.torrent)
            .is_some_and(|t| t.files_selected == 3)
    });
    ctx.shutdown(WHAT);
    drop(seeder);
}

/// The Grab links and Add several links screens' global.
fn links(ui: &AppWindow) -> LinksUi<'_> {
    ui.global::<LinksUi>()
}

/// A folder listing ("Index of /listing/") on the test server, with one
/// subfolder; returns the page's link. Nine files on the page (2 videos,
/// 3 documents, 1 archive, 2 images, 1 program) and a disc image in the
/// subfolder.
fn serve_listing(ctx: &Ctx) -> String {
    const FILES: [(&str, usize); 9] = [
        ("a.mp4", 2000),
        ("b.mkv", 3000),
        ("c.pdf", 4000),
        ("d.txt", 500),
        ("e.docx", 6000),
        ("f.zip", 7000),
        ("g.png", 800),
        ("h.jpg", 900),
        ("i.exe", 10_000),
    ];
    let mut links = String::from(r#"<a href="../">Parent Directory</a> <a href="sub/">sub/</a> "#);
    for (name, size) in FILES {
        ctx.server
            .serve(&format!("listing/{name}"), FileSpec::new(size));
        links.push_str(&format!(r#"<a href="{name}">{name}</a> "#));
    }
    ctx.server.serve("listing/sub/j.iso", FileSpec::new(1200));
    let page = |title: &str, links: &str| {
        format!(
            "<html><head><title>{title}</title></head><body><h1>{title}</h1>{links}</body></html>"
        )
    };
    ctx.server.serve_page(
        "listing/sub/",
        200,
        &[],
        &page(
            "Index of /listing/sub/",
            r#"<a href="../">Parent Directory</a> <a href="j.iso">j.iso</a>"#,
        ),
    );
    ctx.server
        .serve_page("listing/", 200, &[], &page("Index of /listing/", &links))
}

/// The grab rows' names, in order.
fn grab_names(ui: &AppWindow) -> Vec<String> {
    links(ui)
        .get_grab_rows()
        .iter()
        .map(|r| r.name.to_string())
        .collect()
}

/// Advanced on, in a window tall enough that every element is in view.
fn advanced_tall(ui: &AppWindow, what: &str) {
    ui.window().set_size(TALL);
    ui.set_screen(Screen::Downloads);
    ui.invoke_setting_changed("advanced".into(), "true".into());
    assert_eq!(ui.get_layout().as_str(), "compact", "{what}: Advanced");
}

/// Grab links opened from the "+" panel: a folder listing's links are
/// counted by type chip, the Subfolders switch reads it again with its
/// subfolder, and a chip and an extension filter narrow the list.
fn grab_listing_chip_counts(ui: &AppWindow, ctx: &Ctx) {
    const WHAT: &str = "grab_listing_chip_counts";
    let url = serve_listing(ctx);
    advanced_tall(ui, WHAT);
    only_button(ui, "Add a download", WHAT).invoke_accessible_default_action();
    only_button(ui, "Grab links from a page…", WHAT).invoke_accessible_default_action();
    assert_eq!(ui.get_screen(), Screen::Grab, "{WHAT}: not on Grab");
    assert_eq!(
        links(ui).get_grab().page_url.as_str(),
        "",
        "{WHAT}: no page yet"
    );

    links(ui).invoke_grab_page(url.as_str().into());
    wait_until(&format!("{WHAT}: page listed"), || {
        let g = links(ui).get_grab();
        !g.loading && g.count_all == 9
    });
    let g = links(ui).get_grab();
    assert_eq!(g.error_kind.as_str(), "", "{WHAT}: {g:?}");
    let chip_counts = |what: &str| {
        let counts: Vec<(String, String)> = ["All", "Video", "Docs", "Archives", "Images"]
            .iter()
            .map(|label| {
                let chip = only_with_role(ui, AccessibleRole::Tab, label, what);
                let value = chip
                    .accessible_value()
                    .map(|v| v.to_string())
                    .unwrap_or_default();
                (label.to_string(), value)
            })
            .collect();
        counts
    };
    let expect = |all: u32, archives: u32| -> Vec<(String, String)> {
        [
            ("All", all),
            ("Video", 2),
            ("Docs", 3),
            ("Archives", archives),
            ("Images", 2),
        ]
        .iter()
        .map(|(l, n)| (l.to_string(), n.to_string()))
        .collect()
    };
    assert_eq!(chip_counts(WHAT), expect(9, 1), "{WHAT}: chips");
    assert_eq!(grab_names(ui).len(), 9, "{WHAT}: rows");

    let switch = only_with_role(ui, AccessibleRole::Switch, "Subfolders", WHAT);
    switch.invoke_accessible_default_action();
    assert!(links(ui).get_grab().subfolders, "{WHAT}: Subfolders on");
    wait_until(&format!("{WHAT}: subfolder read"), || {
        let g = links(ui).get_grab();
        !g.loading && g.count_all == 10
    });
    assert_eq!(chip_counts(WHAT), expect(10, 2), "{WHAT}: chips with sub/");
    let iso = links(ui)
        .get_grab_rows()
        .iter()
        .find(|r| r.name == "j.iso")
        .unwrap_or_else(|| panic!("{WHAT}: no j.iso"));
    assert_eq!(iso.folder.as_str(), "/sub/", "{WHAT}: folder");

    only_with_role(ui, AccessibleRole::Tab, "Docs", WHAT).invoke_accessible_default_action();
    assert_eq!(grab_names(ui), ["c.pdf", "d.txt", "e.docx"], "{WHAT}: Docs");
    links(ui).invoke_grab_option("ext".into(), ".PDF zip".into());
    assert_eq!(grab_names(ui), ["c.pdf"], "{WHAT}: Docs + pdf");
    only_with_role(ui, AccessibleRole::Tab, "All", WHAT).invoke_accessible_default_action();
    assert_eq!(grab_names(ui), ["c.pdf", "f.zip"], "{WHAT}: All + pdf zip");
    links(ui).invoke_grab_option("ext".into(), "".into());
    assert_eq!(grab_names(ui).len(), 10, "{WHAT}: no filter");

    only_button(ui, "Back", WHAT).invoke_accessible_default_action();
    assert_ne!(ui.get_screen(), Screen::Grab, "{WHAT}: still on Grab");
    assert_eq!(
        links(ui).get_grab_rows().row_count(),
        0,
        "{WHAT}: rows kept"
    );
}

/// "Download 3" with three links ticked through the list adds three web
/// downloads, which finish.
fn grab_download_adds_three(ui: &AppWindow, ctx: &Ctx) {
    const WHAT: &str = "grab_download_adds_three";
    let url = format!("{}/listing/", ctx.server.base_url());
    let before = ctx.engine.snapshot().len();
    advanced_tall(ui, WHAT);
    ui.invoke_open_grab(url.as_str().into());
    assert_eq!(ui.get_screen(), Screen::Grab, "{WHAT}: not on Grab");
    wait_until(&format!("{WHAT}: page listed"), || {
        let g = links(ui).get_grab();
        !g.loading && g.count_all == 9
    });
    for name in ["a.mp4", "c.pdf", "f.zip"] {
        only_with_role(ui, AccessibleRole::Checkbox, name, WHAT).invoke_accessible_default_action();
    }
    let g = links(ui).get_grab();
    assert_eq!(g.selected_count, 3, "{WHAT}: {g:?}");
    let ticked: Vec<String> = links(ui)
        .get_grab_rows()
        .iter()
        .filter(|r| r.selected)
        .map(|r| r.name.to_string())
        .collect();
    assert_eq!(ticked, ["a.mp4", "c.pdf", "f.zip"], "{WHAT}: ticked rows");
    // The footer counts the sizes once they are known (2000 + 4000 + 7000).
    wait_until(&format!("{WHAT}: selected size"), || {
        links(ui).get_grab().selected_size == "13 KB"
    });
    only_button(ui, "Download 3", WHAT).invoke_accessible_default_action();
    assert_eq!(
        ui.get_screen(),
        Screen::Downloads,
        "{WHAT}: not on the list"
    );
    for name in ["a.mp4", "c.pdf", "f.zip"] {
        wait_until(&format!("{WHAT}: {name} finished"), || {
            job_named(ctx, name).is_some_and(|v| v.state == JobState::Completed)
        });
    }
    assert_eq!(
        ctx.engine.snapshot().len(),
        before + 3,
        "{WHAT}: jobs added"
    );
    ui.invoke_setting_changed("advanced".into(), "false".into());
    ui.window().set_size(COMPACT);
}

/// Review Focus 4 (ruling R8): a page of 500 links lists all 500 rows
/// before any size is known; the sizes (each probe answered slowly) then
/// fill in over many later ticks, row by row, and no call to
/// `update_timers_and_animations` takes more than 200 ms (50 ms is the
/// aim; the longest is logged). Another page read while sizes are still
/// coming gets none of the old page's sizes.
fn grab_500_links_streams_sizes(ui: &AppWindow, ctx: &Ctx) {
    const WHAT: &str = "grab_500_links_streams_sizes";
    const N: usize = 500;
    let mut anchors = String::new();
    for i in 0..N {
        let mut spec = FileSpec::new(1000 + i);
        spec.delay = Some(Duration::from_millis(15));
        ctx.server.serve(&format!("g500/f{i:03}.bin"), spec);
        anchors.push_str(&format!(r#"<a href="f{i:03}.bin">f{i:03}.bin</a>"#));
    }
    let url = ctx.server.serve_page(
        "g500/page.html",
        200,
        &[],
        &format!("<html><body>{anchors}</body></html>"),
    );
    advanced_tall(ui, WHAT);
    ui.invoke_open_grab(url.as_str().into());

    let longest = std::cell::Cell::new(Duration::ZERO);
    let tick = || {
        let start = Instant::now();
        slint::platform::update_timers_and_animations();
        longest.set(longest.get().max(start.elapsed()));
        std::thread::sleep(Duration::from_millis(10));
    };
    let probed = |ui: &AppWindow| {
        links(ui)
            .get_grab_rows()
            .iter()
            .filter(|r| r.probed)
            .count()
    };
    let deadline = Instant::now() + Duration::from_secs(30);
    while links(ui).get_grab_rows().row_count() < N {
        assert!(Instant::now() < deadline, "{WHAT}: rows never listed");
        tick();
    }
    let rows = links(ui).get_grab_rows();
    assert_eq!(rows.row_count(), N, "{WHAT}: rows");
    assert!(
        rows.iter().all(|r| !r.probed && r.size_text.is_empty()),
        "{WHAT}: a size arrived with the rows"
    );
    assert!(!links(ui).get_grab().truncated, "{WHAT}: truncated");
    // Every link ticked before any size is known: the footer says so.
    links(ui).invoke_grab_select_shown();
    let g = links(ui).get_grab();
    assert_eq!(
        (g.selected_count, g.unknown_count),
        (500, 500),
        "{WHAT}: {g:?}"
    );
    assert!(
        !texts_containing(ui, "500 selected · size unknown").is_empty(),
        "{WHAT}: footer before sizes"
    );
    let mut said_at_least = false;

    // Sizes fill in over many ticks.
    let mut seen = vec![0usize];
    let deadline = Instant::now() + Duration::from_secs(60);
    while *seen.last().unwrap() < N {
        assert!(
            Instant::now() < deadline,
            "{WHAT}: only {} of {N} sizes",
            seen.last().unwrap()
        );
        tick();
        let now = probed(ui);
        if now != *seen.last().unwrap() {
            seen.push(now);
        }
        if !said_at_least && now > 0 && now < N && links(ui).get_grab().unknown_count > 0 {
            assert!(
                !texts_containing(ui, "500 selected · at least").is_empty(),
                "{WHAT}: footer while sizes come"
            );
            said_at_least = true;
        }
    }
    assert!(said_at_least, "{WHAT}: no partial footer seen");
    eprintln!(
        "{WHAT}: sizes arrived in {} steps; longest update_timers_and_animations call {:?} (aim 50 ms)",
        seen.len() - 1,
        longest.get()
    );
    assert!(seen.len() > 4, "{WHAT}: sizes arrived in {seen:?}");
    assert!(
        longest.get() <= Duration::from_millis(200),
        "{WHAT}: a timer call took {:?}",
        longest.get()
    );
    let rows = links(ui).get_grab_rows();
    assert_eq!(rows.row_count(), N, "{WHAT}: rows rebuilt");
    let first = rows.row_data(0).unwrap();
    assert_eq!(
        (first.name.as_str(), first.size_text.as_str()),
        ("f000.bin", "1.0 KB"),
        "{WHAT}: first row"
    );
    let last = rows.row_data(N - 1).unwrap();
    assert_eq!(last.size_text.as_str(), "1.5 KB", "{WHAT}: last row");
    // 1000 + ... + 1499 bytes, all known.
    wait_until(&format!("{WHAT}: footer total"), || {
        links(ui).get_grab().unknown_count == 0
    });
    assert!(
        !texts_containing(ui, "500 selected · 625 KB").is_empty(),
        "{WHAT}: footer total"
    );

    // A new page while the old one's sizes are still coming: only the
    // new page's sizes are shown.
    links(ui).invoke_grab_page(url.as_str().into());
    let deadline = Instant::now() + Duration::from_secs(30);
    while !(links(ui).get_grab_rows().row_count() == N && probed(ui) > 0) {
        assert!(Instant::now() < deadline, "{WHAT}: second run");
        tick();
    }
    let mut small = FileSpec::new(5_000_000);
    small.delay = Some(Duration::from_millis(5));
    ctx.server.serve("g2/one.bin", small.clone());
    ctx.server.serve("g2/two.bin", small);
    let other = ctx.server.serve_page(
        "g2/index.html",
        200,
        &[],
        r#"<html><body><a href="one.bin">one</a> <a href="two.bin">two</a></body></html>"#,
    );
    links(ui).invoke_grab_page(other.as_str().into());
    let sizes = |ui: &AppWindow| -> Vec<String> {
        links(ui)
            .get_grab_rows()
            .iter()
            .map(|r| r.size_text.to_string())
            .collect()
    };
    let deadline = Instant::now() + Duration::from_secs(30);
    while sizes(ui) != ["5.0 MB", "5.0 MB"] {
        assert!(
            Instant::now() < deadline,
            "{WHAT}: new page sizes {:?}",
            sizes(ui)
        );
        let rows = links(ui).get_grab_rows();
        assert!(rows.row_count() <= 2, "{WHAT}: old rows back");
        assert!(
            rows.iter()
                .all(|r| r.size_text.is_empty() || r.size_text == "5.0 MB"),
            "{WHAT}: an old size reached the new page: {:?}",
            sizes(ui)
        );
        tick();
    }
    settle(Duration::from_millis(300));
    assert_eq!(sizes(ui), ["5.0 MB", "5.0 MB"], "{WHAT}: sizes changed");
    links(ui).invoke_grab_closed();
    ui.invoke_setting_changed("advanced".into(), "false".into());
    ui.window().set_size(COMPACT);
}

/// Add several links, opened from the "+" panel: the same file on two
/// servers, with "Treat copies as mirrors" on, is one download with one
/// mirror; a web page line offers "Find files →", which opens Grab links.
fn batch_copies_become_mirrors(ui: &AppWindow, ctx: &Ctx) {
    const WHAT: &str = "batch_copies_become_mirrors";
    let spec = FileSpec::new(512 * 1024);
    let first = ctx.server.serve("bm/twin.bin", spec.clone());
    let mirror = ctx.rt.block_on(TestServer::start());
    let second = mirror.serve("elsewhere/TWIN.bin", spec);
    let page = format!("{}/listing/", ctx.server.base_url());
    advanced_tall(ui, WHAT);
    only_button(ui, "Add a download", WHAT).invoke_accessible_default_action();
    only_button(ui, "Several links…", WHAT).invoke_accessible_default_action();
    assert_eq!(
        ui.get_screen(),
        Screen::Batch,
        "{WHAT}: not on Several links"
    );
    only_with_role(ui, AccessibleRole::TextInput, "Links", WHAT);

    links(ui)
        .invoke_batch_edited(format!("# two copies\n{first}\n\n{second}\nnot a link\n").into());
    let lines = |ui: &AppWindow| -> Vec<(i32, String, i32)> {
        links(ui)
            .get_batch_lines()
            .iter()
            .map(|l| (l.n, l.kind.to_string(), l.mirror_of))
            .collect()
    };
    assert_eq!(
        lines(ui),
        [
            (2, "http".to_string(), 0),
            (4, "http".to_string(), 0),
            (5, "invalid".to_string(), 0)
        ],
        "{WHAT}: lines"
    );
    assert_eq!(links(ui).get_batch().add_count, 2, "{WHAT}: two downloads");
    for (text, kind) in [(first.as_str(), "File"), ("not a link", "Not a link")] {
        let line = only_with_role(ui, AccessibleRole::ListItem, text, WHAT);
        assert_eq!(
            line.accessible_value().map(|v| v.to_string()).as_deref(),
            Some(kind),
            "{WHAT}: {text} read as"
        );
    }
    only_with_role(ui, AccessibleRole::Switch, "Treat copies as mirrors", WHAT)
        .invoke_accessible_default_action();
    assert!(links(ui).get_batch().mirrors, "{WHAT}: mirrors on");
    assert_eq!(lines(ui)[1], (4, "http".to_string(), 2), "{WHAT}: mirror");
    assert_eq!(links(ui).get_batch().add_count, 1, "{WHAT}: one download");

    // A page line: "Find files →" opens Grab links with it.
    links(ui).invoke_batch_edited(format!("{page}\n").into());
    assert_eq!(lines(ui), [(1, "page".to_string(), 0)], "{WHAT}: page line");
    only_button(ui, "Find files →", WHAT).invoke_accessible_default_action();
    assert_eq!(ui.get_screen(), Screen::Grab, "{WHAT}: Grab not opened");
    assert_eq!(
        links(ui).get_grab().page_url.as_str(),
        page,
        "{WHAT}: page link"
    );
    wait_until(&format!("{WHAT}: page listed"), || {
        let g = links(ui).get_grab();
        !g.loading && g.count_all == 9
    });
    links(ui).invoke_grab_closed();

    let before = ctx.engine.snapshot().len();
    ui.invoke_open_batch();
    assert_eq!(
        ui.get_screen(),
        Screen::Batch,
        "{WHAT}: not on Several links"
    );
    assert_eq!(
        links(ui).get_batch_lines().row_count(),
        0,
        "{WHAT}: old lines"
    );
    links(ui).invoke_batch_edited(format!("{first}\n{second}\n").into());
    links(ui).invoke_batch_mirrors(true);
    only_button(ui, "Add 1 download", WHAT).invoke_accessible_default_action();
    assert_eq!(
        ui.get_screen(),
        Screen::Downloads,
        "{WHAT}: not on the list"
    );
    wait_until(&format!("{WHAT}: twin.bin finished"), || {
        job_named(ctx, "twin.bin").is_some_and(|v| v.state == JobState::Completed)
    });
    settle(Duration::from_millis(300));
    assert_eq!(ctx.engine.snapshot().len(), before + 1, "{WHAT}: one job");
    let id = job_named(ctx, "twin.bin").unwrap().id;
    let detail = {
        let _rt = ctx.rt.enter();
        ctx.engine.job_detail(id).unwrap()
    };
    assert_eq!(detail.mirrors, [second], "{WHAT}: mirrors");
    ui.invoke_setting_changed("advanced".into(), "false".into());
    ui.window().set_size(COMPACT);
}

/// The Add screen offers "Find files →" for a web page link; it leaves
/// the Add screen for Grab links with that link.
fn add_page_offers_find_files(ui: &AppWindow, ctx: &Ctx) {
    const WHAT: &str = "add_page_offers_find_files";
    let page = format!("{}/listing/", ctx.server.base_url());
    advanced_tall(ui, WHAT);
    // As `link_entered` leaves it for a page link that failed to read.
    ui.set_adv_page_url(page.as_str().into());
    ui.set_adv_add(taurus_desktop::AdvAddVm {
        error_kind: "site_not_supported".into(),
        checksum_ok: true,
        ..Default::default()
    });
    ui.set_screen(Screen::Add);
    only_button(ui, "Find files →", WHAT).invoke_accessible_default_action();
    assert_eq!(ui.get_screen(), Screen::Grab, "{WHAT}: Grab not opened");
    assert_eq!(
        links(ui).get_grab().page_url.as_str(),
        page,
        "{WHAT}: page link"
    );
    wait_until(&format!("{WHAT}: page listed"), || {
        let g = links(ui).get_grab();
        !g.loading && g.count_all == 9
    });
    links(ui).invoke_grab_closed();
    ui.set_adv_page_url("".into());
    ui.invoke_setting_changed("advanced".into(), "false".into());
    ui.window().set_size(COMPACT);
}

/// Grabbed files keep the listing's subfolders: `a/x.pdf` and `b/x.pdf`
/// land in their own folders (of the sort folder). A subfolder whose name
/// is not safe to create (`c\d`) is named in the footer, and its file
/// alone is left out. The Subfolders switch reads the link as typed in the
/// field, not the page read before.
fn grab_keeps_subfolders(ui: &AppWindow, ctx: &Ctx) {
    const WHAT: &str = "grab_keeps_subfolders";
    let listing = format!("{}/listing/", ctx.server.base_url());
    ctx.server.serve("twin/a/x.pdf", FileSpec::new(3000));
    ctx.server.serve("twin/b/x.pdf", FileSpec::new(4000));
    // Asked for as `c%5Cd/`, which the server reads as `c\d/`.
    ctx.server.serve("twin/c\\d/y.pdf", FileSpec::new(5000));
    let index = |title: &str, links: &str| {
        format!(
            "<html><head><title>{title}</title></head><body><h1>{title}</h1>{links}</body></html>"
        )
    };
    for (dir, file) in [("a", "x.pdf"), ("b", "x.pdf"), ("c\\d", "y.pdf")] {
        ctx.server.serve_page(
            &format!("twin/{dir}/"),
            200,
            &[],
            &index(
                "Index of a folder",
                &format!(r#"<a href="{file}">{file}</a>"#),
            ),
        );
    }
    let twin = ctx.server.serve_page(
        "twin/",
        200,
        &[],
        &index(
            "Index of /twin/",
            r#"<a href="../">Parent</a> <a href="a/">a/</a> <a href="b/">b/</a> <a href="c%5Cd/">c\d/</a>"#,
        ),
    );
    advanced_tall(ui, WHAT);
    ui.invoke_open_grab(listing.as_str().into());
    wait_until(&format!("{WHAT}: listing read"), || {
        let g = links(ui).get_grab();
        !g.loading && g.count_all == 9
    });
    // Typed in the field, not entered: the switch reads it.
    let field = only_with_role(ui, AccessibleRole::TextInput, "Page link", WHAT);
    field.set_accessible_value(twin.as_str());
    assert_eq!(
        links(ui).get_grab_url_text().as_str(),
        twin,
        "{WHAT}: field"
    );
    only_with_role(ui, AccessibleRole::Switch, "Subfolders", WHAT)
        .invoke_accessible_default_action();
    wait_until(&format!("{WHAT}: twin read with subfolders"), || {
        let g = links(ui).get_grab();
        !g.loading && g.page_url == twin && g.count_all == 3
    });
    let mut rows: Vec<(String, String)> = links(ui)
        .get_grab_rows()
        .iter()
        .map(|r| (r.folder.to_string(), r.name.to_string()))
        .collect();
    rows.sort();
    assert_eq!(
        rows,
        [
            ("/a/".to_string(), "x.pdf".to_string()),
            ("/b/".to_string(), "x.pdf".to_string()),
            ("/c\\d/".to_string(), "y.pdf".to_string()),
        ],
        "{WHAT}: rows"
    );
    only_button(ui, "Select all shown", WHAT).invoke_accessible_default_action();
    let g = links(ui).get_grab();
    assert_eq!((g.selected_count, g.unsafe_count), (3, 1), "{WHAT}: {g:?}");
    assert!(
        !texts_containing(ui, "1 ticked file is in a folder whose name isn't safe").is_empty(),
        "{WHAT}: no unsafe note"
    );
    let before = ctx.engine.snapshot().len();
    only_button(ui, "Download 3", WHAT).invoke_accessible_default_action();
    assert_eq!(
        ui.get_screen(),
        Screen::Downloads,
        "{WHAT}: not on the list"
    );
    // The engine records `dest_dir` canonicalized (`/private/var/...` on
    // macOS, long names instead of `RUNNER~1` on Windows): folders are
    // compared canonicalized on both sides.
    let same_dir = |a: &std::path::Path, b: &std::path::Path| {
        matches!(
            (std::fs::canonicalize(a), std::fs::canonicalize(b)),
            (Ok(a), Ok(b)) if a == b
        )
    };
    for (dir, size) in [("a", 3000), ("b", 4000)] {
        let folder = ctx.documents().join(dir);
        let file = folder.join("x.pdf");
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            slint::platform::update_timers_and_animations();
            let landed = std::fs::metadata(&file).is_ok_and(|m| m.len() == size);
            let done = ctx.engine.snapshot().iter().any(|v| {
                v.url.ends_with(&format!("/twin/{dir}/x.pdf"))
                    && v.state == JobState::Completed
                    && same_dir(&v.dest_dir, &folder)
            });
            if landed && done {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "{WHAT}: {} timed out (file there: {landed}); jobs: {}",
                file.display(),
                jobs_dump(ctx)
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    settle(Duration::from_millis(300));
    assert_eq!(
        ctx.engine.snapshot().len(),
        before + 2,
        "{WHAT}: jobs added"
    );
    assert!(job_named(ctx, "y.pdf").is_none(), "{WHAT}: y.pdf added");
    ui.invoke_setting_changed("advanced".into(), "false".into());
    ui.window().set_size(COMPACT);
}

/// Walks the element tree, so `if` blocks that opened or closed are
/// rebuilt before anything is asserted about them.
fn refresh_tree(ui: &AppWindow) {
    let _ = ElementQuery::from_root(ui).match_descendants().find_all();
}

fn whendone_choice(ui: &AppWindow, label: &str, what: &str) -> ElementHandle {
    refresh_tree(ui);
    only_with_role(ui, AccessibleRole::RadioButton, label, what)
}

/// Waits until the window shows when-done choice `value` (the engine's
/// echo of an earlier change can still be on its way and put an older
/// choice back for an instant), then rebuilds the `if` blocks.
fn whendone_shown(ui: &AppWindow, value: &str, what: &str) {
    let state = ui.global::<WhenDoneUi>();
    wait_until(&format!("{what}: window shows {value}"), || {
        state.get_when_done().as_str() == value
    });
    refresh_tree(ui);
}

/// The footer's when-done button opens the popover: the choice (Escape
/// closes it) is saved and relabels the button, the sleep note shows for
/// sleep and shutdown only, a custom speed is typed in MB/s (2.5 is
/// 2_500_000 bytes), and a value that is no speed shows an error and
/// changes nothing.
fn whendone_menu_sets_when_done_and_speed(ui: &AppWindow, ctx: &Ctx) {
    const WHAT: &str = "whendone_menu_sets_when_done_and_speed";
    const NOTE: &str = "You get 60 seconds to cancel";
    let state = ui.global::<WhenDoneUi>();
    advanced_tall(ui, WHAT);
    assert_eq!(ctx.engine.settings().when_done, WhenDone::Nothing, "{WHAT}");
    // The engine's settings reach the window in their own time (an older
    // echo can trail): the footer button's label is looked up only once
    // the window shows the choice, with `if` blocks rebuilt.
    whendone_shown(ui, "nothing", WHAT);
    only_button(ui, "When done", WHAT).invoke_accessible_default_action();
    refresh_tree(ui);
    assert!(state.get_open(), "{WHAT}: not open");
    // Escape closes it.
    ui.window().dispatch_event(WindowEvent::KeyPressed {
        text: slint::platform::Key::Escape.into(),
    });
    assert!(!state.get_open(), "{WHAT}: Escape did not close");
    refresh_tree(ui);
    assert!(
        labelled(ui, "Do nothing").is_empty(),
        "{WHAT}: still drawn after Escape"
    );

    only_button(ui, "When done", WHAT).invoke_accessible_default_action();
    for label in [
        "Do nothing",
        "Just notify me",
        "Put the computer to sleep",
        "Shut down",
        "Quit Hyades",
    ] {
        whendone_choice(ui, label, WHAT);
    }
    for label in ["Gentle", "Balanced", "Fastest", "Custom"] {
        only_with_role(ui, AccessibleRole::RadioButton, label, WHAT);
    }
    assert!(texts_containing(ui, NOTE).is_empty(), "{WHAT}: early note");

    // Then sleep: saved, the footer says so, the note shows.
    whendone_choice(ui, "Put the computer to sleep", WHAT).invoke_accessible_default_action();
    wait_until(&format!("{WHAT}: sleep saved"), || {
        ctx.engine.settings().when_done == WhenDone::Sleep
    });
    whendone_shown(ui, "sleep", WHAT);
    assert!(!texts_containing(ui, NOTE).is_empty(), "{WHAT}: sleep note");
    only_button(ui, "Then sleep", WHAT);
    assert!(labelled(ui, "When done").is_empty(), "{WHAT}: old label");
    whendone_choice(ui, "Shut down", WHAT).invoke_accessible_default_action();
    whendone_shown(ui, "shutdown", WHAT);
    assert!(
        !texts_containing(ui, NOTE).is_empty(),
        "{WHAT}: shutdown note"
    );
    only_button(ui, "Then shut down", WHAT);
    whendone_choice(ui, "Quit Hyades", WHAT).invoke_accessible_default_action();
    wait_until(&format!("{WHAT}: quit saved"), || {
        ctx.engine.settings().when_done == WhenDone::Quit
    });
    whendone_shown(ui, "quit", WHAT);
    assert!(texts_containing(ui, NOTE).is_empty(), "{WHAT}: quit note");
    only_button(ui, "Then quit", WHAT);
    whendone_choice(ui, "Just notify me", WHAT).invoke_accessible_default_action();
    whendone_shown(ui, "notify", WHAT);
    only_button(ui, "Notify", WHAT);
    whendone_choice(ui, "Do nothing", WHAT).invoke_accessible_default_action();
    wait_until(&format!("{WHAT}: nothing saved"), || {
        ctx.engine.settings().when_done == WhenDone::Nothing
    });
    whendone_shown(ui, "nothing", WHAT);
    only_button(ui, "When done", WHAT);

    // Presets, then Custom.
    only_with_role(ui, AccessibleRole::RadioButton, "Gentle", WHAT)
        .invoke_accessible_default_action();
    wait_until(&format!("{WHAT}: gentle saved"), || {
        ctx.engine.settings().speed == SpeedLimit::Gentle
    });
    only_with_role(ui, AccessibleRole::RadioButton, "Custom", WHAT)
        .invoke_accessible_default_action();
    refresh_tree(ui);
    let field = only_with_role(ui, AccessibleRole::TextInput, "Custom speed in MB/s", WHAT);
    assert_eq!(
        ctx.engine.settings().speed,
        SpeedLimit::Gentle,
        "{WHAT}: Custom alone changes nothing"
    );
    // Picking Custom shows the limit in effect (Gentle's share of the peak).
    let gentle = ctx
        .engine
        .effective_speed_limit()
        .expect("Gentle is a limit");
    assert_eq!(
        state.get_custom_text().as_str(),
        (gentle as f64 / 1e6).to_string(),
        "{WHAT}: Custom prefill"
    );
    let enter = || {
        ui.window().dispatch_event(WindowEvent::KeyPressed {
            text: slint::platform::Key::Return.into(),
        });
    };
    // Typing saves nothing: "2", "25" are only checked; Enter writes once.
    let mut events = ctx.engine.subscribe();
    for typed in ["2", "25"] {
        field.set_accessible_value(typed);
    }
    settle(Duration::from_millis(200));
    assert_eq!(
        ctx.engine.settings().speed,
        SpeedLimit::Gentle,
        "{WHAT}: typing wrote"
    );
    enter();
    wait_until(&format!("{WHAT}: 25 saved"), || {
        ctx.engine.settings().speed
            == SpeedLimit::Custom {
                bytes_per_sec: 25_000_000,
            }
    });
    settle(Duration::from_millis(200));
    let mut writes = 0;
    loop {
        match events.try_recv() {
            Ok(Event::SettingsChanged(_)) => writes += 1,
            Ok(_) | Err(TryRecvError::Lagged(_)) => {}
            Err(TryRecvError::Empty | TryRecvError::Closed) => break,
        }
    }
    assert_eq!(writes, 1, "{WHAT}: settings writes for 25");
    field.set_accessible_value("2.5");
    enter();
    wait_until(&format!("{WHAT}: custom saved"), || {
        ctx.engine.settings().speed
            == SpeedLimit::Custom {
                bytes_per_sec: 2_500_000,
            }
    });
    assert!(!state.get_custom_invalid(), "{WHAT}: 2.5 flagged");

    // A comma is refused, "1,000" included.
    for bad in ["1,000", "2,5", "abc", "0", "0.05", "10001"] {
        field.set_accessible_value(bad);
        refresh_tree(ui);
        assert!(state.get_custom_invalid(), "{WHAT}: {bad:?} not flagged");
        assert_eq!(
            state.get_custom_comma(),
            bad.contains(','),
            "{WHAT}: {bad:?}"
        );
        let note = if bad.contains(',') {
            "Use a dot for decimals"
        } else {
            "Enter a speed from 0.1 to 10000 MB/s"
        };
        assert!(
            !texts_containing(ui, note).is_empty(),
            "{WHAT}: {bad:?} error text"
        );
        enter();
        settle(Duration::from_millis(150));
        assert_eq!(
            ctx.engine.settings().speed,
            SpeedLimit::Custom {
                bytes_per_sec: 2_500_000
            },
            "{WHAT}: {bad:?} changed the speed"
        );
    }
    // A radio pick leaves the error: the field still holds bad text.
    whendone_choice(ui, "Shut down", WHAT).invoke_accessible_default_action();
    assert!(state.get_custom_invalid(), "{WHAT}: pick cleared the error");
    whendone_choice(ui, "Do nothing", WHAT).invoke_accessible_default_action();
    field.set_accessible_value("0.1");
    refresh_tree(ui);
    assert!(!state.get_custom_invalid(), "{WHAT}: 0.1 flagged");
    assert!(
        texts_containing(ui, "Enter a speed from").is_empty(),
        "{WHAT}: error text stays"
    );
    enter();
    wait_until(&format!("{WHAT}: 0.1 saved"), || {
        ctx.engine.settings().speed
            == SpeedLimit::Custom {
                bytes_per_sec: 100_000,
            }
    });
    field.set_accessible_value("10000");
    enter();
    wait_until(&format!("{WHAT}: 10000 saved"), || {
        ctx.engine.settings().speed
            == SpeedLimit::Custom {
                bytes_per_sec: 10_000_000_000,
            }
    });

    // Escape closes with the field focused.
    ui.window().dispatch_event(WindowEvent::KeyPressed {
        text: slint::platform::Key::Escape.into(),
    });
    assert!(!state.get_open(), "{WHAT}: Escape with the field focused");
    only_button(ui, "When done", WHAT).invoke_accessible_default_action();
    assert!(state.get_open(), "{WHAT}: reopen");
    // The field holds the old custom value; each switch to Custom refills
    // it with the limit in effect.
    assert_eq!(
        state.get_custom_text().as_str(),
        "10000",
        "{WHAT}: reopened"
    );
    whendone_choice(ui, "Balanced", WHAT).invoke_accessible_default_action();
    wait_until(&format!("{WHAT}: balanced saved"), || {
        ctx.engine.settings().speed == SpeedLimit::Balanced
    });
    let balanced = ctx
        .engine
        .effective_speed_limit()
        .expect("Balanced is a limit");
    whendone_choice(ui, "Custom", WHAT).invoke_accessible_default_action();
    assert_eq!(
        state.get_custom_text().as_str(),
        (balanced as f64 / 1e6).to_string(),
        "{WHAT}: refilled with Balanced's limit"
    );
    // Fastest has no limit: the field is empty.
    whendone_choice(ui, "Fastest", WHAT).invoke_accessible_default_action();
    wait_until(&format!("{WHAT}: fastest saved"), || {
        ctx.engine.settings().speed == SpeedLimit::Fastest
    });
    whendone_choice(ui, "Custom", WHAT).invoke_accessible_default_action();
    assert_eq!(state.get_custom_text().as_str(), "", "{WHAT}: Fastest");
    // Leaving the field after an edit writes once (Tab moves focus on).
    let mut events = ctx.engine.subscribe();
    refresh_tree(ui);
    only_with_role(ui, AccessibleRole::TextInput, "Custom speed in MB/s", WHAT)
        .set_accessible_value("7");
    ui.window().dispatch_event(WindowEvent::KeyPressed {
        text: slint::platform::Key::Tab.into(),
    });
    wait_until(&format!("{WHAT}: 7 saved on focus out"), || {
        ctx.engine.settings().speed
            == SpeedLimit::Custom {
                bytes_per_sec: 7_000_000,
            }
    });
    settle(Duration::from_millis(200));
    let mut writes = 0;
    loop {
        match events.try_recv() {
            Ok(Event::SettingsChanged(_)) => writes += 1,
            Ok(_) | Err(TryRecvError::Lagged(_)) => {}
            Err(TryRecvError::Empty | TryRecvError::Closed) => break,
        }
    }
    assert_eq!(writes, 1, "{WHAT}: settings writes on focus out");

    // The footer's speed text is the jobs' total, not the limit.
    assert_eq!(ui.get_speed_down_text().as_str(), "0 B/s", "{WHAT}: idle");
    state.set_open(false);
    refresh_tree(ui);
    assert!(
        labelled(ui, "Do nothing").is_empty(),
        "{WHAT}: still drawn once closed"
    );
    let mut back = ctx.engine.settings();
    back.speed = SpeedLimit::Fastest;
    let engine = ctx.engine.clone();
    ctx.rt.block_on(engine.set_settings(back)).unwrap();
    // The UI's copy takes the engine's echo first: the mode switch below
    // saves that copy, and must not write the custom speed back.
    wait_until(&format!("{WHAT}: fastest echoed"), || {
        ui.get_settings().speed == "fastest"
    });
    ui.invoke_setting_changed("advanced".into(), "false".into());
    ui.window().set_size(COMPACT);
}

/// Tall enough for every section of the compact Advanced Settings page.
const SETTINGS_TALL: slint::LogicalSize = slint::LogicalSize::new(420.0, 3600.0);
const SETTINGS_WIDE: slint::LogicalSize = slint::LogicalSize::new(1200.0, 1600.0);
const SECTIONS: [&str; 7] = [
    "Downloads",
    "Torrents",
    "Network",
    "Engines",
    "Appearance",
    "Privacy",
    "General",
];

/// The labels of every text element drawn (one walk of the tree).
fn drawn_texts(ui: &AppWindow) -> Vec<String> {
    ElementQuery::from_root(ui)
        .match_descendants()
        .match_accessible_role(AccessibleRole::Text)
        .find_all()
        .into_iter()
        .filter(|e| e.size().width > 0.0)
        .filter_map(|e| e.accessible_label())
        .map(|label| label.to_string())
        .collect()
}

/// A text element labelled `label` is drawn.
fn shows_text(ui: &AppWindow, label: &str) -> bool {
    drawn_texts(ui).iter().any(|t| t == label)
}

/// Advanced mode's Settings button opens Advanced Settings: every section
/// on one page (compact) or picked from the list (wide); stopping sharing
/// "never", a custom speed (no comma, applied on Enter), a proxy the
/// engine refuses (its reason shows, the setting stays), the Jade accent,
/// and forgetting the password of one protected download.
fn adv_settings_sections_and_values(ui: &AppWindow, ctx: &Ctx) {
    const WHAT: &str = "adv_settings_sections_and_values";
    advanced_tall(ui, WHAT);
    ui.window().set_size(SETTINGS_TALL);
    only_button(ui, "Settings", WHAT).invoke_accessible_default_action();
    assert_eq!(ui.get_screen(), Screen::Settings, "{WHAT}: not on Settings");
    refresh_tree(ui);
    assert!(
        labelled(ui, "Links and startup").is_empty(),
        "{WHAT}: Simple's settings shown in Advanced mode"
    );
    let texts = drawn_texts(ui);
    for section in SECTIONS {
        assert!(
            texts.iter().any(|t| t == section),
            "{WHAT}: compact: no {section:?}"
        );
    }
    let radio = |label: &str| only_with_role(ui, AccessibleRole::RadioButton, label, WHAT);

    // Stop sharing at ratio: Never stop.
    assert_eq!(
        ctx.engine.settings().torrent.stop_ratio,
        Some(1.0),
        "{WHAT}"
    );
    radio("Never stop").invoke_accessible_default_action();
    wait_until(&format!("{WHAT}: ratio never saved"), || {
        ctx.engine.settings().torrent.stop_ratio.is_none()
    });

    // The custom speed follows R13: a comma is refused, Enter applies.
    let enter = || {
        ui.window().dispatch_event(WindowEvent::KeyPressed {
            text: slint::platform::Key::Return.into(),
        });
    };
    let speed_before = ctx.engine.settings().speed;
    radio("Custom").invoke_accessible_default_action();
    refresh_tree(ui);
    let field = only_with_role(ui, AccessibleRole::TextInput, "Custom speed in MB/s", WHAT);
    field.set_accessible_value("1,5");
    enter();
    assert!(
        shows_text(ui, "Use a dot for decimals, like 2.5."),
        "{WHAT}: comma"
    );
    assert_eq!(
        ctx.engine.settings().speed,
        speed_before,
        "{WHAT}: comma saved"
    );
    field.set_accessible_value("1.5");
    enter();
    wait_until(&format!("{WHAT}: custom speed saved"), || {
        ctx.engine.settings().speed
            == SpeedLimit::Custom {
                bytes_per_sec: 1_500_000,
            }
    });
    radio("Fastest").invoke_accessible_default_action();
    wait_until(&format!("{WHAT}: fastest saved"), || {
        ctx.engine.settings().speed == SpeedLimit::Fastest
    });

    // A connection count set elsewhere shows as it is; a pick replaces it.
    let five = Settings {
        connections: 5,
        ..ctx.engine.settings()
    };
    ctx.rt.block_on(ctx.engine.set_settings(five)).unwrap();
    wait_until(&format!("{WHAT}: custom connections shown"), || {
        refresh_tree(ui);
        labelled(ui, "Custom 5")
            .first()
            .is_some_and(|e| e.accessible_checked() == Some(true))
    });
    radio("16").invoke_accessible_default_action();
    wait_until(&format!("{WHAT}: 16 connections saved"), || {
        ctx.engine.settings().connections == 16
    });
    wait_until(&format!("{WHAT}: custom connections gone"), || {
        refresh_tree(ui);
        labelled(ui, "Custom 5").is_empty()
    });

    // A proxy the engine refuses: its words under the field, nothing saved.
    let proxy_before = ctx.engine.settings().network.proxy;
    radio("Manual").invoke_accessible_default_action();
    refresh_tree(ui);
    let field = only_with_role(ui, AccessibleRole::TextInput, "Proxy URL", WHAT);
    field.set_accessible_value("localhost:1");
    enter();
    wait_until(&format!("{WHAT}: proxy refused"), || {
        ui.global::<taurus_desktop::SettingsUi>().get_proxy_error() != ""
    });
    let reason = ui
        .global::<taurus_desktop::SettingsUi>()
        .get_proxy_error()
        .to_string();
    assert!(
        reason.starts_with("unsupported proxy"),
        "{WHAT}: {reason:?}"
    );
    assert!(shows_text(ui, &reason), "{WHAT}: reason not shown");
    settle(Duration::from_millis(200));
    assert_eq!(
        ctx.engine.settings().network.proxy,
        proxy_before,
        "{WHAT}: proxy changed"
    );
    // Manual is only pending until a URL is applied: the saved proxy stays
    // the one marked; a URL the engine takes makes Manual the one.
    // "System" is also a theme: the Network section's comes first.
    let checked = |label: &str| {
        labelled(ui, label)
            .into_iter()
            .find(|e| e.accessible_role() == Some(AccessibleRole::RadioButton))
            .and_then(|e| e.accessible_checked())
            == Some(true)
    };
    assert!(checked("System") && !checked("Manual"), "{WHAT}: pending");
    assert!(
        shows_text(ui, "Not in use yet: press Enter to use this proxy."),
        "{WHAT}: pending note"
    );
    field.set_accessible_value("http://127.0.0.1:1");
    enter();
    wait_until(&format!("{WHAT}: manual proxy saved"), || {
        ctx.engine.settings().network.proxy
            == ProxySetting::Manual {
                url: "http://127.0.0.1:1".into(),
            }
    });
    refresh_tree(ui);
    assert!(checked("Manual") && !checked("System"), "{WHAT}: manual");
    labelled(ui, "System")
        .into_iter()
        .find(|e| e.accessible_role() == Some(AccessibleRole::RadioButton))
        .expect("the proxy's System")
        .invoke_accessible_default_action();
    wait_until(&format!("{WHAT}: system proxy again"), || {
        ctx.engine.settings().network.proxy == ProxySetting::System
    });

    // A step of a number box is saved; a later change from elsewhere shows
    // in the box, and in a typed field with no edit pending.
    let state = ui.global::<taurus_desktop::SettingsUi>();
    let spin = only_with_role(
        ui,
        AccessibleRole::Spinbox,
        "Active downloads at once",
        WHAT,
    );
    spin.invoke_accessible_increment_action();
    wait_until(&format!("{WHAT}: max active stepped"), || {
        ctx.engine.settings().max_active == 4
    });
    let mut outside = ctx.engine.settings();
    outside.max_active = 7;
    outside.torrent.listen_port = Some(7000);
    ctx.rt.block_on(ctx.engine.set_settings(outside)).unwrap();
    wait_until(&format!("{WHAT}: outside change shown"), || {
        state.get_vm().max_active == 7
            && spin.accessible_value().as_deref() == Some("7")
            && state.get_port_text() == "7000"
    });
    let mut back = ctx.engine.settings();
    back.max_active = 3;
    back.torrent.listen_port = None;
    ctx.rt.block_on(ctx.engine.set_settings(back)).unwrap();
    wait_until(&format!("{WHAT}: put back"), || {
        state.get_vm().max_active == 3 && state.get_port_text() == ""
    });

    // Accent Jade.
    radio("Jade").invoke_accessible_default_action();
    wait_until(&format!("{WHAT}: Jade saved"), || {
        ctx.engine.settings().ui.accent == "Jade"
    });
    let accent = ui.global::<Theme>().get_accent();
    assert!(accent.eq_ignore_ascii_case("Jade"), "{WHAT}: {accent:?}");

    // After one protected download (earlier checks' passwords forgotten
    // first), Forget removes its password.
    ctx.rt
        .block_on(ctx.engine.forget_saved_passwords())
        .unwrap();
    let mut protected = FileSpec::new(64 * 1024);
    protected.basic_auth = Some(("settings-user".into(), "settings-Pa55".into()));
    let request = AddRequest {
        url: ctx.server.serve("settings-locked.bin", protected),
        dest_dir: ctx.out(),
        credentials: Some(taurus_core::Credentials {
            username: "settings-user".into(),
            password: "settings-Pa55".into(),
        }),
        ..Default::default()
    };
    ctx.rt.block_on(ctx.engine.add(request)).unwrap();
    wait_until(&format!("{WHAT}: protected download finished"), || {
        job_named(ctx, "settings-locked.bin").is_some_and(|v| v.state == JobState::Completed)
    });
    only_button(ui, "Forget saved passwords", WHAT).invoke_accessible_default_action();
    let settings_ui = ui.global::<taurus_desktop::SettingsUi>();
    wait_until(&format!("{WHAT}: forgotten"), || {
        settings_ui.get_forget_state() == "done"
    });
    assert_eq!(settings_ui.get_forget_count(), 1, "{WHAT}: removed");
    assert!(
        shows_text(ui, "Removed 1"),
        "{WHAT}: \"Removed 1\" not shown"
    );

    // Wide: the section list, one section at a time.
    ui.window().set_size(SETTINGS_WIDE);
    wait_until(&format!("{WHAT}: wide"), || ui.get_layout() == "wide");
    for (section, control, role) in [
        ("Torrents", "Listening port", AccessibleRole::TextInput),
        ("Network", "System", AccessibleRole::RadioButton),
        ("Engines", "Check now", AccessibleRole::Button),
        ("Appearance", "Jade", AccessibleRole::RadioButton),
        ("Privacy", "Forget saved passwords", AccessibleRole::Button),
        ("General", "Show advanced options", AccessibleRole::Switch),
        (
            "Downloads",
            "Verify checksums automatically",
            AccessibleRole::Switch,
        ),
    ] {
        refresh_tree(ui);
        only_with_role(ui, AccessibleRole::ListItem, section, WHAT)
            .invoke_accessible_default_action();
        only_with_role(ui, role, control, WHAT);
        let texts = drawn_texts(ui);
        let shown: Vec<&str> = SECTIONS
            .into_iter()
            .filter(|s| texts.iter().any(|t| t == s))
            .collect();
        assert_eq!(shown, [section], "{WHAT}: wide: sections shown");
    }

    // An edit not applied yet is applied when the section changes (as
    // Enter would), or flagged and kept when it is no value it takes; the
    // field never shows an unsaved value as if it were saved.
    let section = |name: &str| {
        refresh_tree(ui);
        only_with_role(ui, AccessibleRole::ListItem, name, WHAT).invoke_accessible_default_action();
        refresh_tree(ui);
    };
    let port_field = || only_with_role(ui, AccessibleRole::TextInput, "Listening port", WHAT);
    section("Torrents");
    port_field().set_accessible_value("6000");
    section("Network");
    wait_until(&format!("{WHAT}: port applied on section change"), || {
        ctx.engine.settings().torrent.listen_port == Some(6000)
    });
    section("Torrents");
    assert_eq!(
        port_field().accessible_value().as_deref(),
        Some("6000"),
        "{WHAT}: port field"
    );
    port_field().set_accessible_value("80");
    section("Network");
    section("Torrents");
    assert_eq!(
        ctx.engine.settings().torrent.listen_port,
        Some(6000),
        "{WHAT}: bad port saved"
    );
    assert_eq!(port_field().accessible_value().as_deref(), Some("80"));
    assert!(state.get_port_invalid(), "{WHAT}: bad port not flagged");
    port_field().set_accessible_value("");
    section("Downloads");
    wait_until(&format!("{WHAT}: random port again"), || {
        ctx.engine.settings().torrent.listen_port.is_none()
    });
    only_button(ui, "Back", WHAT).invoke_accessible_default_action();
    assert_ne!(ui.get_screen(), Screen::Settings, "{WHAT}: Back");
    ui.invoke_setting_changed("advanced".into(), "false".into());
    ui.window().set_size(COMPACT);
}

/// The yt-dlp row's version on the Engines section.
fn ytdlp_row_version(ui: &AppWindow) -> String {
    ui.global::<taurus_desktop::SettingsUi>()
        .get_tools()
        .iter()
        .find(|t| t.kind == "yt-dlp")
        .map(|t| t.version.to_string())
        .unwrap_or_default()
}

fn engine_ytdlp_version(m: &MediaCtx) -> Option<String> {
    m.ctx
        .engine
        .tools_status()
        .into_iter()
        .find(|s| s.kind == taurus_core::tools::ToolKind::YtDlp)
        .and_then(|s| s.version)
}

/// Engines: "Check now" fetches a newer yt-dlp, "Roll back yt-dlp" makes
/// the version before it active again; the row follows.
fn adv_settings_engines_roll_back(m: &MediaCtx) {
    const WHAT: &str = "adv_settings_engines_roll_back";
    let ui = &m.ui;
    ui.window().set_size(SETTINGS_TALL);
    ui.invoke_setting_changed("advanced".into(), "true".into());
    ui.set_screen(Screen::Settings);
    refresh_tree(ui);
    let first = engine_ytdlp_version(m).expect("yt-dlp installed by the media checks");
    wait_until(&format!("{WHAT}: row shows {first}"), || {
        ytdlp_row_version(ui) == first
    });
    assert!(shows_text(ui, &format!("Version {first}")), "{WHAT}");

    let newer = unique_version();
    set_ytdlp_scenario(&newer, &media_scenario().to_string());
    m.host.set_release("yt-dlp", ToolRelease::new(&newer));
    only_button(ui, "Check now", WHAT).invoke_accessible_default_action();
    wait_for(TOOLS_WAIT, &format!("{WHAT}: {newer} active"), || {
        engine_ytdlp_version(m).as_deref() == Some(newer.as_str()) && ytdlp_row_version(ui) == newer
    });
    wait_until(&format!("{WHAT}: check finished"), || {
        !ui.global::<taurus_desktop::SettingsUi>().get_tools_busy()
    });
    refresh_tree(ui);
    only_button(ui, "Roll back yt-dlp", WHAT).invoke_accessible_default_action();
    wait_until(&format!("{WHAT}: {first} active again"), || {
        engine_ytdlp_version(m).as_deref() == Some(first.as_str()) && ytdlp_row_version(ui) == first
    });
    assert_eq!(
        ui.global::<taurus_desktop::SettingsUi>()
            .get_tools_error()
            .as_str(),
        "",
        "{WHAT}: error shown"
    );
    ui.set_screen(Screen::Downloads);
    ui.invoke_setting_changed("advanced".into(), "false".into());
    ui.window().set_size(COMPACT);
}

/// Every job's link, folder, name, state and error, for failure messages.
fn jobs_dump(ctx: &Ctx) -> String {
    ctx.engine
        .snapshot()
        .iter()
        .map(|v| {
            format!(
                "\n  {} -> {} / {} [{:?}] {:?}",
                v.url,
                v.dest_dir.display(),
                v.file_name,
                v.state,
                v.error
            )
        })
        .collect()
}

/// Presses and lets go of `key` (a `slint::platform::Key` or a character).
fn press(ui: &AppWindow, key: impl Into<slint::SharedString>) {
    let text = key.into();
    ui.window()
        .dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
    ui.window()
        .dispatch_event(WindowEvent::KeyReleased { text });
}

/// Shift+Tab.
fn press_shift_tab(ui: &AppWindow) {
    use slint::platform::Key;
    ui.window().dispatch_event(WindowEvent::KeyPressed {
        text: Key::Shift.into(),
    });
    press(ui, Key::Tab);
    ui.window().dispatch_event(WindowEvent::KeyReleased {
        text: Key::Shift.into(),
    });
}

/// Keyboard use of Advanced mode, on a window of its own (no engine: the
/// callbacks are recorded). Tab goes through the header, the tabs, each
/// row then its action, and the footer; Escape closes the "+" panel and
/// gives the focus back to "+". Each dialog takes the keys when it opens:
/// Escape cancels it, Enter confirms it, Tab goes round its controls
/// without leaving it. Escape goes back from the compact detail (after a
/// dialog over it closed, too), Grab links, Several links, Advanced add
/// and Advanced settings, also from a text field.
fn keyboard_tab_enter_escape() {
    use slint::platform::Key;
    const WHAT: &str = "keyboard_tab_enter_escape";
    let ui = AppWindow::new().unwrap();
    ui.show().unwrap();
    ui.window().set_size(TALL);
    ui.set_layout("compact".into());
    ui.set_screen(Screen::Downloads);
    let calls: Rc<RefCell<Vec<String>>> = Rc::default();
    let log = |name: &'static str| {
        let calls = calls.clone();
        move |id: i32| calls.borrow_mut().push(format!("{name} {id}"))
    };
    ui.on_select(log("select"));
    ui.on_pause(log("pause"));
    ui.on_resume(log("resume"));
    ui.on_tab_changed(log("tab"));
    let weak = ui.as_weak();
    ui.on_open_whendone(move || {
        weak.unwrap().global::<WhenDoneUi>().set_open(true);
    });
    let taken = || std::mem::take(&mut *calls.borrow_mut());
    let rows = |rows: Vec<RowVm>| {
        let rows: Vec<AdvRowVm> = rows
            .into_iter()
            .map(|row| AdvRowVm {
                row,
                source: "HTTP".into(),
                ..Default::default()
            })
            .collect();
        ModelRc::new(VecModel::from(rows))
    };
    ui.set_adv_rows(rows(vec![row(1, "downloading"), row(2, "paused")]));
    let shows = |label: &str| {
        refresh_tree(&ui);
        !labelled(&ui, label).is_empty()
    };
    refresh_tree(&ui);

    // The first Tab reaches "+": it opens the link field, Escape closes
    // it and "+" has the focus again.
    press(&ui, Key::Tab);
    press(&ui, " ");
    assert!(
        shows("Paste a link"),
        "{WHAT}: \"+\" did not open the field"
    );
    press(&ui, Key::Escape);
    assert!(!shows("Paste a link"), "{WHAT}: Escape left the field open");
    press(&ui, " ");
    assert!(shows("Paste a link"), "{WHAT}: \"+\" lost the focus");
    press(&ui, Key::Escape);
    assert!(!shows("Paste a link"), "{WHAT}: field open again");

    // Then Settings, the three tabs, and each row followed by its action.
    // Space presses what has the focus.
    for _ in 0..5 {
        press(&ui, Key::Tab);
    }
    for _ in 0..4 {
        press(&ui, " ");
        press(&ui, Key::Tab);
    }
    assert_eq!(
        taken(),
        ["select 1", "pause 1", "select 2", "resume 2"],
        "{WHAT}: Tab order through the rows"
    );
    // The footer's when-done button comes next.
    press(&ui, " ");
    assert!(ui.global::<WhenDoneUi>().get_open(), "{WHAT}: when done");
    press(&ui, Key::Escape);
    assert!(!ui.global::<WhenDoneUi>().get_open(), "{WHAT}: popover");
    refresh_tree(&ui);

    keyboard_remove_dialog(&ui, &calls);
    keyboard_password_dialog(&ui, &calls);
    keyboard_files_dialog(&ui, &calls);
    keyboard_screens(&ui, &calls);
}

/// The wide layout's detail pane holds a 200-character name, a long
/// folder and link, every action row and the sharing selects within its
/// width: nothing in it is wider than the pane (no sideways scrolling).
fn wide_detail_holds_long_names() {
    const WHAT: &str = "wide_detail_holds_long_names";
    let ui = AppWindow::new().unwrap();
    ui.show().unwrap();
    ui.window()
        .set_size(slint::LogicalSize::new(1100.0, 1600.0));
    ui.set_layout("wide".into());
    ui.set_screen(Screen::Downloads);
    let name: String = "Popeye_forPresident_512kb_"
        .repeat(8)
        .chars()
        .take(196)
        .chain(".mp4".chars())
        .collect();
    assert_eq!(name.chars().count(), 200);
    let stat = |key: &str, value: &str| taurus_desktop::StatVm {
        key: key.into(),
        value: value.into(),
    };
    ui.set_detail(DetailVm {
        visible: true,
        row: row(1, "paused"),
        name: name.as_str().into(),
        kind: "media".into(),
        source: "Torrent".into(),
        big: "50%".into(),
        size_line: "13 MB / 27 MB".into(),
        cells: ModelRc::new(VecModel::from(vec![0.5f32; 96])),
        has_cells: true,
        files: ModelRc::new(VecModel::from(vec![taurus_desktop::FileRowVm {
            index: 0,
            name: name.as_str().into(),
            size_text: "27 MB".into(),
            progress: 0.5,
            selected: true,
        }])),
        stats: ModelRc::new(VecModel::from(vec![
            stat("checksum", &format!("sha256:{}", "ab".repeat(32))),
            stat("password", "memory"),
        ])),
        folder: format!("/tmp/{}", "a-very-long-folder-name-".repeat(6)).into(),
        url: format!(
            "https://archive.org/download/{}/{name}",
            "Popeye".repeat(12)
        )
        .into(),
        can_convert: true,
        can_retry_cookies: true,
        can_choose_files: true,
        ratio_override: "default".into(),
        seed_time_override: "default".into(),
        ..Default::default()
    });
    refresh_tree(&ui);
    only_button(&ui, "Convert…", WHAT).invoke_accessible_default_action();
    only_button(&ui, "Use my browser's sign-in", WHAT).invoke_accessible_default_action();
    refresh_tree(&ui);
    let pane = ElementHandle::find_by_element_type_name(&ui, "DetailPane")
        .next()
        .expect("the detail pane");
    let right = pane.absolute_position().x + pane.size().width;
    assert!(pane.size().width <= 300.5, "{WHAT}: pane {:?}", pane.size());
    let wide: Vec<String> = pane
        .query_descendants()
        .find_all()
        .into_iter()
        .filter(|e| e.size().width > 0.0)
        .filter(|e| e.absolute_position().x + e.size().width > right + 0.5)
        .map(|e| {
            format!(
                "{:?} {:?} {}",
                e.type_name(),
                e.accessible_label(),
                e.size().width
            )
        })
        .collect();
    assert!(wide.is_empty(), "{WHAT}: wider than the pane: {wide:?}");
    // The name is all there, on several lines.
    assert!(
        labelled(&ui, &name)
            .iter()
            .any(|e| e.accessible_role() == Some(AccessibleRole::Text) && e.size().height > 40.0),
        "{WHAT}: the name is not wrapped"
    );
    assert!(
        shows_text(&ui, "Password: Saved until Hyades quits"),
        "{WHAT}: where the password is kept"
    );
    ui.hide().unwrap();
}

/// The Advanced add of a torrent with many files, at 420x640: the file
/// list shrinks to what the window has left, so the screen itself does
/// not scroll ("Start immediately" shows without scrolling) beside the
/// list's own scrollbar.
fn torrent_add_scrolls_only_its_list() {
    const WHAT: &str = "torrent_add_scrolls_only_its_list";
    let ui = AppWindow::new().unwrap();
    ui.show().unwrap();
    ui.window().set_size(COMPACT);
    ui.set_layout("compact".into());
    ui.set_screen(Screen::Add);
    let files: Vec<TorrentFileVm> = (0..100)
        .map(|i| TorrentFileVm {
            index: i,
            name: format!("file-{i:03}.bin").into(),
            path: format!("file-{i:03}.bin").into(),
            size_text: "1 KB".into(),
            selected: true,
            state: "all".into(),
            ..Default::default()
        })
        .collect();
    ui.set_adv_add(taurus_desktop::AdvAddVm {
        kind: "torrent".into(),
        badge: "magnet".into(),
        name: "pack".into(),
        size_text: "100 KB".into(),
        files: ModelRc::new(VecModel::from(files)),
        selected_count: 100,
        selected_size: "100 KB".into(),
        ratio: "default".into(),
        checksum_ok: true,
        folder: "/tmp/dl".into(),
        free_text: "1 GB".into(),
        ..Default::default()
    });
    refresh_tree(&ui);
    let start = only_with_role(&ui, AccessibleRole::Checkbox, "Start immediately", WHAT);
    let bottom = start.absolute_position().y + start.size().height;
    assert!(bottom <= 640.0, "{WHAT}: below the window: {bottom}");
    let rows = ElementQuery::from_root(&ui)
        .match_descendants()
        .match_accessible_role(AccessibleRole::Checkbox)
        .find_all()
        .into_iter()
        .filter(|e| e.accessible_label().is_some_and(|l| l.starts_with("file-")))
        .count();
    assert!(rows >= 3, "{WHAT}: {rows} file rows shown");
    ui.hide().unwrap();
}

type Calls = Rc<RefCell<Vec<String>>>;

/// Remove: Escape cancels, Enter removes (finished files kept), Tab goes
/// box, Cancel, Remove and round to the box; Shift+Tab round to Remove.
fn keyboard_remove_dialog(ui: &AppWindow, calls: &Calls) {
    use slint::platform::Key;
    const WHAT: &str = "keyboard_remove_dialog";
    let taken = || std::mem::take(&mut *calls.borrow_mut());
    let (log, weak) = (calls.clone(), ui.as_weak());
    ui.on_confirm_remove(move |id, delete| {
        log.borrow_mut().push(format!("remove {id} {delete}"));
        let ui = weak.unwrap();
        ui.set_remove_dialog(RemoveDialogVm::default());
    });
    let open = || {
        ui.set_remove_dialog(RemoveDialogVm {
            visible: true,
            id: 7,
            name: "a.bin".into(),
        });
        refresh_tree(ui);
    };
    let ticked = || {
        only_with_role(ui, AccessibleRole::Checkbox, "Also delete the files", WHAT)
            .accessible_checked()
    };
    open();
    press(ui, Key::Escape);
    assert!(!ui.get_remove_dialog().visible, "{WHAT}: Escape");
    open();
    press(ui, Key::Return);
    assert_eq!(taken(), ["remove 7 false"], "{WHAT}: Enter");
    open();
    press(ui, Key::Tab);
    press(ui, " ");
    assert_eq!(ticked(), Some(true), "{WHAT}: first Tab not on the box");
    for _ in 0..3 {
        press(ui, Key::Tab);
    }
    press(ui, " ");
    assert_eq!(ticked(), Some(false), "{WHAT}: Tab did not go round");
    press_shift_tab(ui);
    press(ui, " ");
    assert_eq!(
        taken(),
        ["remove 7 false"],
        "{WHAT}: Shift+Tab not on Remove"
    );
    assert!(!ui.get_remove_dialog().visible, "{WHAT}: still open");

    // Return on the box ticks it and back; it never removes.
    open();
    press(ui, Key::Tab);
    press(ui, Key::Return);
    assert_eq!(ticked(), Some(true), "{WHAT}: Return did not tick");
    assert!(ui.get_remove_dialog().visible, "{WHAT}: Return on the box");
    press(ui, Key::Return);
    assert_eq!(ticked(), Some(false), "{WHAT}: Return did not untick");
    assert_eq!(taken(), Vec::<String>::new(), "{WHAT}: Return on the box");
    press(ui, Key::Escape);

    // Enter held on "Remove…": its press opens the dialog; the repeats
    // neither confirm nor close it; a new press does.
    let weak = ui.as_weak();
    ui.on_ask_remove(move |id| {
        weak.unwrap().set_remove_dialog(RemoveDialogVm {
            visible: true,
            id,
            name: "a.bin".into(),
        });
    });
    ui.set_detail(DetailVm {
        visible: true,
        row: row(7, "paused"),
        name: "a.bin".into(),
        ..Default::default()
    });
    refresh_tree(ui);
    only_button(ui, "Remove…", WHAT).invoke_accessible_default_action();
    refresh_tree(ui);
    assert!(
        ui.get_remove_dialog().visible,
        "{WHAT}: Remove… did not ask"
    );
    for _ in 0..3 {
        ui.window().dispatch_event(WindowEvent::KeyPressRepeated {
            text: Key::Return.into(),
        });
    }
    assert_eq!(taken(), Vec::<String>::new(), "{WHAT}: a repeat removed");
    assert!(ui.get_remove_dialog().visible, "{WHAT}: a repeat closed it");
    ui.window().dispatch_event(WindowEvent::KeyReleased {
        text: Key::Return.into(),
    });
    press(ui, Key::Return);
    assert_eq!(taken(), ["remove 7 false"], "{WHAT}: a new Enter");
    ui.set_detail(DetailVm::default());
    refresh_tree(ui);
}

/// Sign in: the user name field has the focus; Escape from it cancels and
/// empties both fields; Enter there moves on to the password, Enter there
/// signs in; Tab goes user name, password, Cancel (Sign in is off while a
/// field is empty) and round; Shift+Tab from the user name to Cancel.
fn keyboard_password_dialog(ui: &AppWindow, calls: &Calls) {
    use slint::platform::Key;
    const WHAT: &str = "keyboard_password_dialog";
    let taken = || std::mem::take(&mut *calls.borrow_mut());
    let (log, weak) = (calls.clone(), ui.as_weak());
    ui.on_submit_password(move |id, user, password| {
        log.borrow_mut()
            .push(format!("sign in {id} {user} {password}"));
        let ui = weak.unwrap();
        ui.set_password_user("".into());
        ui.set_password_text("".into());
        ui.set_password_dialog(PasswordDialogVm::default());
    });
    let open = || {
        ui.set_password_dialog(PasswordDialogVm {
            visible: true,
            id: 9,
            name: "locked.bin".into(),
            ..Default::default()
        });
        refresh_tree(ui);
    };
    let typed = || {
        (
            ui.get_password_user().to_string(),
            ui.get_password_text().to_string(),
        )
    };
    open();
    press(ui, "u");
    assert_eq!(typed(), ("u".into(), String::new()), "{WHAT}: focus");
    press(ui, Key::Escape);
    assert!(!ui.get_password_dialog().visible, "{WHAT}: Escape");
    assert_eq!(typed(), (String::new(), String::new()), "{WHAT}: emptied");
    open();
    press(ui, "u");
    press(ui, Key::Return);
    press(ui, "p");
    assert_eq!(typed(), ("u".into(), "p".into()), "{WHAT}: Enter moves on");
    press(ui, Key::Return);
    assert_eq!(taken(), ["sign in 9 u p"], "{WHAT}: Enter signs in");
    open();
    for _ in 0..3 {
        press(ui, Key::Tab);
    }
    press(ui, "x");
    assert_eq!(typed(), ("x".into(), String::new()), "{WHAT}: Tab round");
    press_shift_tab(ui);
    press(ui, " ");
    assert!(!ui.get_password_dialog().visible, "{WHAT}: not on Cancel");
    assert_eq!(taken(), Vec::<String>::new(), "{WHAT}: signed in");
}

/// "Choose files…": Escape cancels, Enter uses the files picked; Tab
/// starts at "All", Shift+Tab from there goes round to "Use these files".
fn keyboard_files_dialog(ui: &AppWindow, calls: &Calls) {
    use slint::platform::Key;
    const WHAT: &str = "keyboard_files_dialog";
    let taken = || std::mem::take(&mut *calls.borrow_mut());
    let log = calls.clone();
    ui.on_files_chosen(move |id, chosen| {
        let chosen: Vec<i32> = chosen.iter().collect();
        log.borrow_mut().push(format!("files {id} {chosen:?}"));
    });
    let log = calls.clone();
    ui.on_adv_toggle_all(move |on| log.borrow_mut().push(format!("all {on}")));
    let file = |index: i32, name: &str| TorrentFileVm {
        index,
        name: name.into(),
        path: name.into(),
        size_text: "1 KB".into(),
        selected: index == 0,
        state: if index == 0 { "all" } else { "none" }.into(),
        ..Default::default()
    };
    let open = || {
        ui.set_files_dialog(FilesDialogVm {
            visible: true,
            id: 5,
            name: "pack".into(),
            files: ModelRc::new(VecModel::from(vec![file(0, "a.bin"), file(1, "b.bin")])),
            selected_count: 1,
            selected_size: "1 KB".into(),
            chosen: ModelRc::new(VecModel::from(vec![0])),
            ..Default::default()
        });
        refresh_tree(ui);
    };
    open();
    press(ui, Key::Escape);
    assert!(!ui.get_files_dialog().visible, "{WHAT}: Escape");
    assert_eq!(taken(), Vec::<String>::new(), "{WHAT}: Escape used them");
    open();
    press(ui, Key::Return);
    assert_eq!(taken(), ["files 5 [0]"], "{WHAT}: Enter");
    assert!(!ui.get_files_dialog().visible, "{WHAT}: open after Enter");
    open();
    press(ui, Key::Tab);
    press(ui, " ");
    press_shift_tab(ui);
    press(ui, " ");
    assert_eq!(
        taken(),
        ["all true", "files 5 [0]"],
        "{WHAT}: Tab to All, Shift+Tab round"
    );
}

/// Escape goes back from Grab links (from its link field: Tab goes Back,
/// then the field), Several links (from its links box), Advanced add and
/// Advanced settings; and from the compact detail, also once a dialog
/// over it was closed with Escape.
fn keyboard_screens(ui: &AppWindow, calls: &Calls) {
    use slint::platform::Key;
    const WHAT: &str = "keyboard_screens";
    let taken = || std::mem::take(&mut *calls.borrow_mut());
    let note = |name: &'static str| {
        let log = calls.clone();
        move || log.borrow_mut().push(name.to_string())
    };
    links(ui).on_grab_closed(note("grab closed"));
    ui.global::<SettingsUi>().on_leave(note("settings left"));
    ui.on_add_cancelled(note("add cancelled"));
    let back_from = |screen: Screen, field: Option<&str>, called: &[&str]| {
        ui.set_screen(screen);
        refresh_tree(ui);
        if let Some(label) = field {
            press(ui, Key::Tab);
            press(ui, Key::Tab);
            press(ui, "h");
            let value = only_with_role(ui, AccessibleRole::TextInput, label, WHAT)
                .accessible_value()
                .unwrap_or_default();
            assert_eq!(value.as_str(), "h", "{WHAT}: {screen:?}: Tab to {label}");
        }
        press(ui, Key::Escape);
        assert_eq!(ui.get_screen(), Screen::Welcome, "{WHAT}: {screen:?}");
        assert_eq!(taken(), called, "{WHAT}: {screen:?}");
    };
    back_from(Screen::Grab, Some("Page link"), &["grab closed"]);
    links(ui).set_grab_url_text("".into());
    back_from(Screen::Batch, Some("Links"), &[]);
    back_from(Screen::Add, None, &["add cancelled"]);
    back_from(Screen::Settings, None, &["settings left"]);

    let (log, weak) = (calls.clone(), ui.as_weak());
    ui.on_select(move |id| {
        log.borrow_mut().push(format!("select {id}"));
        let ui = weak.unwrap();
        ui.set_detail(DetailVm::default());
    });
    let open = || {
        ui.set_detail(DetailVm {
            visible: true,
            row: row(3, "paused"),
            name: "file-3.zip".into(),
            ..Default::default()
        });
        refresh_tree(ui);
    };
    ui.set_screen(Screen::Downloads);
    open();
    press(ui, Key::Escape);
    assert_eq!(taken(), ["select -1"], "{WHAT}: detail");
    open();
    ui.set_remove_dialog(RemoveDialogVm {
        visible: true,
        id: 3,
        name: "file-3.zip".into(),
    });
    refresh_tree(ui);
    press(ui, Key::Escape);
    assert!(!ui.get_remove_dialog().visible, "{WHAT}: dialog");
    refresh_tree(ui);
    assert_eq!(taken(), Vec::<String>::new(), "{WHAT}: detail with dialog");
    press(ui, Key::Escape);
    assert_eq!(taken(), ["select -1"], "{WHAT}: detail after the dialog");
}
