//! Desktop notifications. Each is shown from its own short-lived thread so
//! a slow or missing notification service never holds up the window;
//! failures are logged and otherwise ignored.

use crate::shell_text::{ALL_DONE_TITLE, APP_NAME, FAILED_TITLE, FINISHED_TITLE};

/// A download finished.
pub fn finished(name: &str) {
    show(FINISHED_TITLE, name.to_string());
}

/// A download failed; `message` is its `simple_message`.
pub fn failed(name: &str, message: &str) {
    show(FAILED_TITLE, format!("{name}\n{message}"));
}

/// The when-done "Notify me": the queue is done.
pub fn all_done() {
    show(ALL_DONE_TITLE, String::new());
}

fn show(title: &'static str, body: String) {
    let spawned = std::thread::Builder::new()
        .name("notification".into())
        .spawn(move || {
            let result = notify_rust::Notification::new()
                .appname(APP_NAME)
                .summary(title)
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
