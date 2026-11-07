//! Desktop notifications. Each is shown from its own short-lived thread so
//! a slow or missing notification service never holds up the window;
//! failures are logged and otherwise ignored. Titles come from
//! `ShellStrings`, read once at start.

use crate::shell_text::{APP_NAME, ShellStrings};

/// A download finished.
pub fn finished(text: &ShellStrings, name: &str) {
    show(text.finished_title.clone(), name.to_string());
}

/// A download failed; `message` is its `ShellStrings::simple`.
pub fn failed(text: &ShellStrings, name: &str, message: &str) {
    show(text.failed_title.clone(), format!("{name}\n{message}"));
}

/// The when-done "Notify me": the queue is done.
pub fn all_done(text: &ShellStrings) {
    show(text.all_done_title.clone(), String::new());
}

fn show(title: String, body: String) {
    let spawned = std::thread::Builder::new()
        .name("notification".into())
        .spawn(move || {
            let result = notify_rust::Notification::new()
                .appname(APP_NAME)
                .summary(&title)
                .body(&body)
                .show();
            if let Err(e) = result {
                eprintln!("hyades: notification failed: {e}");
            }
        });
    if let Err(e) = spawned {
        eprintln!("hyades: notification failed: {e}");
    }
}
