//! Text the OS shows outside Slint: the tray icon and its menu, desktop
//! notifications, the login item and the message box shown when the app
//! cannot start. The sentences live in the translation catalog
//! (`ShellText` in `ui/shell_text.slint`, `Messages.simple` for errors);
//! `ShellStrings` reads them from the window once the language is applied.
//! Only the product name, which is never translated, is a Rust constant.

use slint::ComponentHandle;
use taurus_core::{CoreError, ErrorKind};

use crate::{AppWindow, Messages, ShellText};

/// Product name: tray tooltip, notification app name, login item name.
pub const APP_NAME: &str = "Hyades";

/// The `ShellText` global in the window's language, one field per
/// property. The language is fixed for the life of the process, so these
/// are read once at start.
#[derive(Clone, Debug, PartialEq)]
pub struct ShellStrings {
    pub tray_open: String,
    pub tray_pause_all: String,
    pub tray_resume_all: String,
    pub tray_quit: String,
    /// Title of the notification for a finished download; its body is the
    /// file name.
    pub finished_title: String,
    /// Title of the notification for a failed download; its body is the
    /// file name, then `simple` of the error on its own line.
    pub failed_title: String,
    /// The when-done "Notify me" notification.
    pub all_done_title: String,
    /// Start-up message box: the command-line program holds the data folder.
    pub open_in_use: String,
    /// Start-up message box: the data folder could not be opened otherwise.
    pub open_failed: String,
}

impl ShellStrings {
    /// Reads the `ShellText` global. Works on a window that is not shown
    /// yet and before the event loop runs.
    pub fn from_ui(ui: &AppWindow) -> ShellStrings {
        let text = ui.global::<ShellText>();
        ShellStrings {
            tray_open: text.get_tray_open().into(),
            tray_pause_all: text.get_tray_pause_all().into(),
            tray_resume_all: text.get_tray_resume_all().into(),
            tray_quit: text.get_tray_quit().into(),
            finished_title: text.get_finished_title().into(),
            failed_title: text.get_failed_title().into(),
            all_done_title: text.get_all_done_title().into(),
            open_in_use: text.get_open_in_use().into(),
            open_failed: text.get_open_failed().into(),
        }
    }

    /// The Simple-mode sentence for an error kind: `Messages.simple` with
    /// the key the rows use (`ErrorKind::as_str`, see `rows::row_of`).
    pub fn simple(ui: &AppWindow, kind: ErrorKind) -> String {
        ui.global::<Messages>()
            .invoke_simple(kind.as_str().into())
            .into()
    }

    /// What the start-up message box says when `Engine::open` failed.
    pub fn open_error(&self, e: &CoreError) -> &str {
        if e.is_instance_locked() {
            &self.open_in_use
        } else {
            &self.open_failed
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings() -> ShellStrings {
        let s = |t: &str| t.to_string();
        ShellStrings {
            tray_open: s("open"),
            tray_pause_all: s("pause"),
            tray_resume_all: s("resume"),
            tray_quit: s("quit"),
            finished_title: s("finished"),
            failed_title: s("failed"),
            all_done_title: s("all done"),
            open_in_use: s("in use"),
            open_failed: s("open failed"),
        }
    }

    #[test]
    fn open_failure_says_why() {
        let shell = strings();
        let other = CoreError {
            kind: ErrorKind::NoPermission,
            detail: "permission denied".into(),
        };
        assert_eq!(shell.open_error(&other), "open failed");

        // The data folder held by another engine (as the CLI would).
        let dir = tempfile::tempdir().unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let config = || taurus_core::EngineConfig::new(dir.path());
            let first = taurus_core::Engine::open(config()).await.unwrap();
            let held = taurus_core::Engine::open(config()).await.err().unwrap();
            assert_eq!(shell.open_error(&held), "in use");
            first.shutdown().await.unwrap();
        });
    }
}
