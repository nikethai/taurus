//! Text the OS shows outside Slint: the tray icon and its menu, desktop
//! notifications, the login item and the message box shown when the app
//! cannot start. Fixed sentences only, each used whole
//! or next to a file name, never built from pieces, so they can move to the
//! translation catalog as they are. Everything inside the window lives in
//! the `.slint` files.

use taurus_core::{CoreError, ErrorKind};

/// Product name: tray tooltip, notification app name, login item name.
pub const APP_NAME: &str = "Hyades";

pub const TRAY_OPEN: &str = "Open Hyades";
pub const TRAY_PAUSE_ALL: &str = "Pause all";
pub const TRAY_RESUME_ALL: &str = "Resume all";
pub const TRAY_QUIT: &str = "Quit";

/// Title of the notification for a finished download; its body is the
/// file name.
pub const FINISHED_TITLE: &str = "Download finished";
/// Title of the notification for a failed download; its body is the file
/// name, then `simple_message` of the error on its own line.
pub const FAILED_TITLE: &str = "Download stopped";
/// The when-done "Notify me" notification.
pub const ALL_DONE_TITLE: &str = "All downloads finished";

/// Start-up message box: the command-line program holds the data folder.
pub const OPEN_IN_USE: &str =
    "Hyades is already running in another program (hyades-cli). Close it, then open Hyades again.";
/// Start-up message box: the data folder could not be opened otherwise.
pub const OPEN_FAILED: &str = "Hyades couldn't open its data folder. Check that the drive is there and that you can write to it, then open Hyades again.";

/// What the start-up message box says when `Engine::open` failed.
pub fn open_failed(e: &CoreError) -> &'static str {
    if e.is_instance_locked() {
        OPEN_IN_USE
    } else {
        OPEN_FAILED
    }
}

/// The Simple-mode sentence for an error kind, word for word what
/// `Messages.simple` in `ui/messages.slint` says (a UI test compares them
/// for every kind).
pub fn simple_message(kind: ErrorKind) -> &'static str {
    match kind {
        ErrorKind::LinkExpired => "This link stopped working. It may have expired.",
        ErrorKind::NeedsSignIn => "This site wants you to sign in first.",
        ErrorKind::BlockedByBotCheck => {
            "This site wants to check you're human. Open it in your browser and start the download there."
        }
        ErrorKind::CaptivePortal => {
            "Your network wants you to sign in (hotel or café Wi-Fi?). Open your browser, then try again."
        }
        ErrorKind::QuotaExceeded => {
            "Too many people downloaded this file recently. Try again later."
        }
        ErrorKind::DiskFull => "Your disk is full. Free up some space, then try again.",
        ErrorKind::FolderUnavailable => "The save folder isn't available. Is the drive plugged in?",
        ErrorKind::NoPermission => {
            "Hyades can't save to this folder. Pick another one in Settings."
        }
        ErrorKind::FileTooLargeForDisk => {
            "This file is too big for that drive. Pick another folder."
        }
        ErrorKind::ChecksumMismatch => {
            "The downloaded file didn't match its checksum, so it was removed. Try again."
        }
        ErrorKind::SiteNotSupported => "This link is a web page, not a file.",
        ErrorKind::UnsafeTorrent => "This torrent has unsafe file names, so it wasn't saved.",
        ErrorKind::AlreadyAdded => "This download is already in your list.",
        ErrorKind::MediaUnavailable => "This video is private or was removed.",
        ErrorKind::MediaRegionLocked => "This video isn't available in your country.",
        ErrorKind::ToolFailed => {
            "The video tools couldn't finish this file. The pieces were kept in the folder."
        }
        ErrorKind::NeedsConfirmation => "This needs your confirmation.",
        ErrorKind::FileChangedOnServer => {
            "The file changed on the server, so the download started over."
        }
        ErrorKind::ConnectionLost
        | ErrorKind::Timeout
        | ErrorKind::RateLimited
        | ErrorKind::ToolMissing
        | ErrorKind::NoPeers
        | ErrorKind::Unknown => "Something went wrong. Try again.",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_failure_says_why() {
        let other = CoreError {
            kind: ErrorKind::NoPermission,
            detail: "permission denied".into(),
        };
        assert_eq!(open_failed(&other), OPEN_FAILED);

        // The data folder held by another engine (as the CLI would).
        let dir = tempfile::tempdir().unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let config = || taurus_core::EngineConfig::new(dir.path());
            let first = taurus_core::Engine::open(config()).await.unwrap();
            let held = taurus_core::Engine::open(config()).await.err().unwrap();
            assert_eq!(open_failed(&held), OPEN_IN_USE);
            first.shutdown().await.unwrap();
        });
    }

    #[test]
    fn every_error_kind_has_a_simple_message() {
        for kind in ErrorKind::ALL {
            let text = simple_message(kind);
            assert!(text.ends_with(['.', '?']), "{kind:?}: {text:?}");
        }
        assert_eq!(
            simple_message(ErrorKind::Unknown),
            "Something went wrong. Try again."
        );
    }
}
