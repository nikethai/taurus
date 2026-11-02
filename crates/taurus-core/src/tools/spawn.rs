//! Starting a tool's process: no console window on Windows, the child
//! killed if its handle is dropped, and a retry where Linux briefly refuses.
//! (`tools::tree` builds on it to start a process together with the
//! processes it starts.)

use std::io;
use std::time::Duration;

use tokio::process::{Child, Command};

/// Attempts at running a binary that exec refuses as busy (ETXTBSY).
const BUSY_ATTEMPTS: u32 = 10;

/// Spawns `command` with `kill_on_drop` set (and `CREATE_NO_WINDOW` and
/// `CREATE_NEW_PROCESS_GROUP` on Windows: the child gets no window, and
/// its own group, so a Ctrl+Break for it reaches only it and its
/// children), retrying for about a second while exec answers ETXTBSY: a binary
/// just written can still be open for writing in a child another thread
/// forked. The caller sets stdio and arguments.
pub(crate) async fn spawn(command: &mut Command) -> io::Result<Child> {
    command.kill_on_drop(true);
    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        command.creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP);
    }
    let mut attempt = 0;
    loop {
        match command.spawn() {
            Err(e) if is_busy(&e) && attempt + 1 < BUSY_ATTEMPTS => {
                attempt += 1;
                tokio::time::sleep(Duration::from_millis(25) * attempt).await;
            }
            result => return result,
        }
    }
}

/// ETXTBSY is 26 on Linux and macOS.
fn is_busy(e: &io::Error) -> bool {
    cfg!(unix) && e.raw_os_error() == Some(26)
}
