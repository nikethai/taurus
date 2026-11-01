//! Keeps the computer from sleeping while something downloads.
//!
//! The inhibitor lives on a thread of its own: on Windows it is a setting of
//! the thread that made it, so it must be made and dropped on one thread,
//! and creating it on Linux is a blocking D-Bus call.

use std::sync::mpsc;

/// Asks the OS not to sleep while idle for as long as it is requested.
/// Best-effort: an OS that refuses (no D-Bus session on a headless Linux,
/// say) is logged, and the wish is still recorded.
pub struct AwakeGuard {
    requested: bool,
    /// Wishes for the thread that holds the inhibitor, started on first need.
    holder: Option<mpsc::Sender<bool>>,
}

impl AwakeGuard {
    pub fn new() -> Self {
        AwakeGuard {
            requested: false,
            holder: None,
        }
    }

    /// Keeps the computer awake from now on, or lets it sleep again.
    pub fn set(&mut self, want: bool) {
        if want == self.requested {
            return;
        }
        self.requested = want;
        if self.holder.is_none() && want {
            self.holder = spawn_holder();
        }
        if let Some(holder) = &self.holder
            && holder.send(want).is_err()
        {
            // Its thread died; the next wish starts another.
            tracing::warn!("could not reach the keep-awake thread");
            self.holder = None;
        }
    }

    /// Whether keeping the computer awake is requested, whether or not the
    /// OS agreed.
    pub fn is_requested(&self) -> bool {
        self.requested
    }
}

impl Default for AwakeGuard {
    fn default() -> Self {
        AwakeGuard::new()
    }
}

fn spawn_holder() -> Option<mpsc::Sender<bool>> {
    let (wishes, rx) = mpsc::channel();
    match std::thread::Builder::new()
        .name("taurus-keep-awake".to_string())
        .spawn(move || hold(&rx))
    {
        Ok(_) => Some(wishes),
        Err(e) => {
            tracing::warn!("could not start the keep-awake thread: {e}");
            None
        }
    }
}

/// Holds an inhibitor while the last wish is `true`; ends, releasing it,
/// when the guard is dropped. Wishes alternate, so a new inhibitor never
/// replaces a live one.
fn hold(wishes: &mpsc::Receiver<bool>) {
    let mut _inhibitor = None;
    for want in wishes {
        _inhibitor = if want { inhibit() } else { None };
    }
}

fn inhibit() -> Option<keepawake::KeepAwake> {
    keepawake::Builder::default()
        .idle(true)
        .reason("Downloading files")
        .app_name("Hyades")
        .app_reverse_domain("dev.nikethai.hyades")
        .create()
        .inspect_err(|e| tracing::warn!("could not keep the computer awake: {e}"))
        .ok()
}
