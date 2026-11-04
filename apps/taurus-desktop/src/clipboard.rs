//! Watching the clipboard for copied links, by polling (no clipboard offers
//! change events everywhere), and putting text on it.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Reads the clipboard every `interval` on a new thread and calls `on_text`
/// with its text each time that changes. While `enabled` is false nothing
/// is read at all (copied passwords stay out of the app); the text there
/// at start, or when it turns true again, counts as seen, so an old link
/// is not offered. Without a clipboard (no display, as on a headless
/// machine) it logs once and the thread ends.
pub fn watch(
    interval: Duration,
    enabled: Arc<AtomicBool>,
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
            let is_on = || enabled.load(Ordering::Relaxed);
            let mut was_on = is_on();
            let mut last = if was_on {
                clipboard.get_text().ok()
            } else {
                None
            };
            loop {
                std::thread::sleep(interval);
                let on = is_on();
                let turned_on = on && !was_on;
                was_on = on;
                if !on {
                    continue;
                }
                if turned_on {
                    last = clipboard.get_text().ok();
                    continue;
                }
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

/// Puts text on the clipboard. The clipboard stays open once used: on
/// Linux (X11) copied text is only there while the app holds it.
#[derive(Default)]
pub struct Writer(Option<arboard::Clipboard>);

impl Writer {
    pub fn set(&mut self, text: &str) -> Result<(), arboard::Error> {
        let clipboard = match &mut self.0 {
            Some(c) => c,
            None => self.0.insert(arboard::Clipboard::new()?),
        };
        clipboard.set_text(text)
    }
}
