//! Opening Hyades at login.

use auto_launch::{AutoLaunchBuilder, MacOSLaunchMode, WindowsEnableMode};

use crate::shell_text::APP_NAME;

/// Registers (or removes) this executable as a login item named "Hyades":
/// an XDG autostart entry on Linux, a launch agent on macOS, the current
/// user's Run key on Windows.
pub fn set_open_at_login(enabled: bool) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let launcher = AutoLaunchBuilder::new()
        .set_app_name(APP_NAME)
        .set_app_path(&exe.to_string_lossy())
        .set_macos_launch_mode(MacOSLaunchMode::LaunchAgent)
        // This user only, also when started elevated (no HKLM entry).
        .set_windows_enable_mode(WindowsEnableMode::CurrentUser)
        .build()
        .map_err(|e| e.to_string())?;
    let registered = launcher.is_enabled().unwrap_or(false);
    let result = match (enabled, registered) {
        (true, false) => launcher.enable(),
        (false, true) => launcher.disable(),
        _ => Ok(()),
    };
    result.map_err(|e| e.to_string())
}
