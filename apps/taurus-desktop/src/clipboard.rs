//! Watching the clipboard for copied links, by polling (no clipboard offers
//! change events everywhere).

use std::time::Duration;

/// Reads the clipboard every `interval` on a new thread and calls `on_text`
/// with its text each time that changes (the text there at start counts as
/// seen, so an old link is not offered). Without a clipboard (no display,
/// as on a headless machine) it logs once and the thread ends.
pub fn watch(
    interval: Duration,
    on_text: impl Fn(String) + Send + 'static,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("clipboard".into())
        .spawn(move || {
            let mut clipboard = match arboard::Clipboard::new() {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("hyades: no clipboard, copied links are not offered: {e}");
                    return;
                }
            };
            let mut last = clipboard.get_text().ok();
            loop {
                std::thread::sleep(interval);
                match clipboard.get_text() {
                    Ok(text) if last.as_deref() != Some(text.as_str()) => {
                        last = Some(text.clone());
                        on_text(text);
                    }
                    Ok(_) => {}
                    Err(e @ arboard::Error::ClipboardNotSupported) => {
                        eprintln!("hyades: clipboard stopped: {e}");
                        return;
                    }
                    // Not text (an image, nothing), busy, or a hiccup: try
                    // again later.
                    Err(_) => {}
                }
            }
        })
        .expect("spawn the clipboard thread")
}
