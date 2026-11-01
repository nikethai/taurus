//! What a round shares with its workers: each segment's byte window and the
//! reports workers send back.

use std::sync::{Mutex, MutexGuard};

/// The byte window a worker may still write: `[pos, end)`. The worker
/// reserves bytes (advances `pos`) under the lock before writing them; work
/// stealing lowers `end` under the same lock and only to a split above `pos`.
/// So no byte is written by two workers and a worker never writes past `end`.
pub(super) struct Window {
    pub(super) pos: u64,
    pub(super) end: u64,
}

pub(super) enum Report {
    /// Segment `idx` is durable up to `done`.
    Durable { idx: usize, done: u64 },
    /// A worker started reading a 206 response.
    Opened,
}

pub(super) fn lock(window: &Mutex<Window>) -> MutexGuard<'_, Window> {
    // A worker panic cannot leave the two integers inconsistent.
    window.lock().unwrap_or_else(|p| p.into_inner())
}
