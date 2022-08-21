#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

mod command;
mod model;
mod client;
mod retry;
mod tau_err;

use tauri::Manager;
mod user_agent;
use tauri_awesome_rpc::AwesomeRpc;
use window_vibrancy::{apply_vibrancy, NSVisualEffectMaterial};

fn main() {
    let awesome_rpc = AwesomeRpc::new(vec!["tauri://localhost", "http://localhost:*"]);

    tauri::Builder::default()
        .invoke_system(awesome_rpc.initialization_script(), AwesomeRpc::responder())
        .setup(move |app| {
            let win = app.get_window("main").unwrap();

            awesome_rpc.start(app.handle());

            #[cfg(target_os = "macos")]
            apply_vibrancy(&win, NSVisualEffectMaterial::HudWindow)
                .expect("Unsupported platform! 'apply_vibrancy' is only supported on macOS");

            #[cfg(target_os = "windows")]
            apply_acrylic(&win, Some((18, 18, 18, 125)))
                .expect("Unsupported platform! 'apply_blur' is only supported on Windows");

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            command::greet,
            command::get_download_info,
            command::cancel_get_download_info,
            command::rp_time_elapsed,
            command::get_disk_info,
            command::check_permission,
            command::exists,
            command::download_file,
        ])
        .run(tauri::generate_context!())
        .expect("Error while running tauri application");
}
