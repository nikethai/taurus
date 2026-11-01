// No console window next to the app on Windows.
#![cfg_attr(windows, windows_subsystem = "windows")]

//! Hyades desktop: one instance per data folder, the engine on a tokio
//! runtime, the window on the main thread, and the OS shell around it
//! (tray, notifications, clipboard offers, login item).

use std::cell::OnceCell;
use std::path::PathBuf;
use std::process::ExitCode;
use std::rc::{Rc, Weak};
use std::time::Duration;

use futures_util::StreamExt;
use slint::ComponentHandle;
use taurus_core::{Engine, EngineConfig};
use taurus_desktop::app::App;
use taurus_desktop::instance::{self, Instance};
use taurus_desktop::{AppWindow, clipboard, tray};

const CLIPBOARD_POLL: Duration = Duration::from_secs(1);

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("hyades: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<ExitCode, Box<dyn std::error::Error>> {
    // One spelling of the folder, so every launch finds the same socket.
    let data_dir = data_dir()?;
    std::fs::create_dir_all(&data_dir)?;
    let data_dir = std::fs::canonicalize(&data_dir)?;
    let links: Vec<String> = std::env::args().skip(1).collect();

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let listener = match rt.block_on(instance::claim(&instance::socket_name(&data_dir), &links))? {
        // The running instance took our links (or was asked to show itself).
        Instance::Secondary => return Ok(ExitCode::SUCCESS),
        Instance::Primary(listener) => listener,
    };
    // Accept forwarded links at once: a second launch waits for us.
    let mut forwarded = {
        let _guard = rt.enter();
        listener.into_stream()
    };
    let engine = rt.block_on(Engine::open(EngineConfig::new(&data_dir)))?;

    let ui = AppWindow::new()?;
    let app_cell: Rc<OnceCell<Weak<App>>> = Rc::new(OnceCell::new());
    let tray = tray::build(tray_actions(&app_cell));
    let app = App::start(&ui, engine, rt.handle().clone(), tray.is_some());
    let _ = app_cell.set(Rc::downgrade(&app));
    app.use_os_shell();

    let sender = app.sender();
    rt.spawn(async move {
        while let Some(batch) = forwarded.next().await {
            sender.forwarded(batch);
        }
    });
    if !links.is_empty() {
        app.sender().forwarded(links);
    }
    let sender = app.sender();
    clipboard::watch(CLIPBOARD_POLL, app.offer_links_flag(), move |text| {
        sender.clipboard(text)
    });

    ui.show()?;
    // Not `ui.run()`: hiding the window to the tray must not end the loop.
    slint::run_event_loop_until_quit()?;
    // Quit already stopped the engine; this covers any other way out.
    app.quit();
    drop(tray);
    rt.shutdown_timeout(Duration::from_secs(2));
    Ok(ExitCode::SUCCESS)
}

/// The same folder as the CLI: `TAURUS_DATA_DIR`, else the OS data dir.
fn data_dir() -> Result<PathBuf, String> {
    if let Some(dir) = std::env::var_os("TAURUS_DATA_DIR").filter(|d| !d.is_empty()) {
        return Ok(dir.into());
    }
    directories::ProjectDirs::from("dev", "nikethai", "taurus")
        .map(|dirs| dirs.data_dir().to_path_buf())
        .ok_or_else(|| "no data directory found; set TAURUS_DATA_DIR".to_string())
}

/// Tray actions that reach the app once it exists (the tray is made first
/// so the app knows whether closing may hide the window).
fn tray_actions(app: &Rc<OnceCell<Weak<App>>>) -> tray::TrayActions {
    let call = |f: fn(&App)| {
        let app = app.clone();
        Box::new(move || {
            if let Some(app) = app.get().and_then(Weak::upgrade) {
                f(&app);
            }
        }) as Box<dyn Fn()>
    };
    let paused = app.clone();
    tray::TrayActions {
        open: call(App::show_window),
        pause_all: call(App::pause_all),
        resume_all: call(App::resume_all),
        quit: call(App::quit),
        all_paused: Box::new(move || {
            paused
                .get()
                .and_then(Weak::upgrade)
                .is_some_and(|app| app.all_paused())
        }),
    }
}
