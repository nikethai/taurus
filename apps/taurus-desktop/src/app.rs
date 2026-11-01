//! App-level decisions that do not need a window.

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn close_policy() {
        assert_eq!(super::close_policy(true), ClosePolicy::HideToTray);
        assert_eq!(super::close_policy(false), ClosePolicy::Quit);
    }
}
