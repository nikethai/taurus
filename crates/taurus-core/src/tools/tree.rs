//! A process started together with everything it starts, so that stopping
//! it stops all of it.
//!
//! yt-dlp's release binaries are a PyInstaller bootloader that runs a Python
//! child; ffmpeg runs under yt-dlp. Killing only the process that was
//! started leaves the real work running. A [`TreeChild`] puts the process
//! in a group of its own (a process group on Unix, a Job Object on Windows)
//! and can send the whole group a graceful interrupt or kill it.
//!
//! ```ignore
//! let mut run = TreeChild::spawn(&mut command).await?;   // stdio set by the caller
//! let out = run.child.stdout.take();                     // `child` is a plain tokio Child
//! run.interrupt()?;                                      // Ctrl+C / Ctrl+Break for the group
//! // ... wait a grace period on `run.wait()` ...
//! run.kill().await?;                                     // the whole tree, then reaped
//! ```
//!
//! [`TreeChild::wait`] kills what is left of the tree when the process
//! ends: nothing it started outlives it. Dropping a `TreeChild` whose
//! process has not been reaped kills the tree too (on Windows the job's
//! handle closing does it, even after the process ended). Waiting on
//! `child` directly reaps the process without that, and after it the group
//! is left alone (see [`TreeChild::kill`]).

use std::io;
use std::process::ExitStatus;

use tokio::process::{Child, Command};

use super::spawn::spawn;

/// A running process and its group; see the module documentation.
pub(crate) struct TreeChild {
    /// The started process, to take stdio from and to wait on.
    pub child: Child,
    /// Its pid, which is also the id of its process group (Unix) and the
    /// group a Ctrl+Break is addressed to (Windows).
    pid: u32,
    #[cfg(windows)]
    job: Option<job::Job>,
}

impl TreeChild {
    /// Spawns `command` (via `tools::spawn::spawn`: no window on Windows,
    /// ETXTBSY retried) as the leader of a new group. The caller sets the
    /// arguments and stdio first.
    pub(crate) async fn spawn(command: &mut Command) -> io::Result<TreeChild> {
        #[cfg(unix)]
        command.process_group(0);
        let child = spawn(command).await?;
        let pid = child
            .id()
            .ok_or_else(|| io::Error::other("the process ended before it could be grouped"))?;
        #[cfg(windows)]
        let job = job::Job::containing(&child)
            .map_err(|e| {
                tracing::warn!(
                    "could not put pid {pid} in a job object, so stopping it will not stop \
                     what it starts: {e}"
                )
            })
            .ok();
        Ok(TreeChild {
            child,
            pid,
            #[cfg(windows)]
            job,
        })
    }

    /// The process's id.
    pub(crate) fn pid(&self) -> u32 {
        self.pid
    }

    /// Asks the whole group to stop as Ctrl+C would: SIGINT to the process
    /// group on Unix; CTRL_BREAK_EVENT to the process group on Windows
    /// (yt-dlp, being Python, then raises KeyboardInterrupt). A group that
    /// is already gone is not an error. On Unix nothing is sent once the
    /// process has been reaped (see [`TreeChild::kill`]).
    pub(crate) fn interrupt(&self) -> io::Result<()> {
        #[cfg(unix)]
        {
            if self.reaped() {
                return Ok(());
            }
            signal_group(self.pid, libc::SIGINT)
        }
        #[cfg(windows)]
        {
            use windows_sys::Win32::System::Console::{CTRL_BREAK_EVENT, GenerateConsoleCtrlEvent};
            // SAFETY: plain FFI call with integer arguments.
            if unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, self.pid) } == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }
    }

    /// Kills the whole tree at once (SIGKILL to the group; the job
    /// terminated on Windows) and reaps the started process. Safe to call
    /// at any time, also twice.
    ///
    /// On Unix the group is signalled only while the started process has not
    /// been reaped, and always before it is reaped here: an exited but
    /// unreaped leader still reserves its pid as the group id, but once it
    /// is reaped an empty group's id can be given to an unrelated process
    /// group. So a leader that was reaped (by `child.wait()` or `try_wait()`)
    /// leaves any straggler alone; whoever reaps the leader should kill the
    /// tree first if it wants none left.
    pub(crate) async fn kill(&mut self) -> io::Result<()> {
        self.kill_tree();
        self.child.wait().await.map(|_| ())
    }

    /// Waits for the started process to end, then kills whatever of its
    /// tree is still running and reaps it. A process can end while
    /// something it started does not: a shell whose background job ignores
    /// Ctrl+C, or yt-dlp's bootloader ending before what it started.
    ///
    /// On Unix the end is seen without reaping (`waitid` with `WNOWAIT`),
    /// so that the group is still the process's own when it is killed.
    /// Cancel-safe.
    pub(crate) async fn wait(&mut self) -> io::Result<ExitStatus> {
        #[cfg(unix)]
        if !self.reaped() {
            exited(self.pid).await?;
        }
        #[cfg(windows)]
        self.child.wait().await?;
        self.kill_tree();
        self.child.wait().await
    }

    /// Whether the started process has been reaped, which is when tokio
    /// stops giving its id.
    #[cfg(unix)]
    fn reaped(&self) -> bool {
        self.child.id().is_none()
    }

    fn kill_tree(&mut self) {
        #[cfg(unix)]
        if !self.reaped()
            && let Err(e) = signal_group(self.pid, libc::SIGKILL)
        {
            tracing::warn!("could not kill process group {}: {e}", self.pid);
        }
        #[cfg(windows)]
        if let Some(job) = &self.job
            && let Err(e) = job.terminate()
        {
            tracing::warn!("could not terminate the job of pid {}: {e}", self.pid);
        }
        // The process itself, whichever way the group went (and the Windows
        // case without a job).
        let _ = self.child.start_kill();
    }
}

impl Drop for TreeChild {
    fn drop(&mut self) {
        // `kill_tree` leaves a reaped group alone on Unix; on Windows
        // closing the job (after this) kills what is left anyway.
        self.kill_tree();
    }
}

/// Returns once our child `pid` has ended, leaving it unreaped. A SIGCHLD
/// wakes it; the timeout only bounds how late an end could be noticed
/// should a signal be missed.
#[cfg(unix)]
async fn exited(pid: u32) -> io::Result<()> {
    use tokio::signal::unix::{SignalKind, signal};
    // Listening before the first look: an end after the look is heard.
    let mut sigchld = signal(SignalKind::child())?;
    while !has_exited(pid)? {
        let _ = tokio::time::timeout(std::time::Duration::from_millis(500), sigchld.recv()).await;
    }
    Ok(())
}

/// Whether our child `pid` has ended (or was reaped already), without
/// reaping it.
#[cfg(unix)]
fn has_exited(pid: u32) -> io::Result<bool> {
    let id = libc::id_t::try_from(pid).map_err(io::Error::other)?;
    loop {
        // SAFETY: an all-zero siginfo_t is valid; it outlives the call.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        // SAFETY: plain FFI call with a valid out pointer.
        let r = unsafe {
            libc::waitid(
                libc::P_PID,
                id,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if r == 0 {
            // Left zeroed when it has not ended yet.
            return Ok(info.si_signo == libc::SIGCHLD);
        }
        let e = io::Error::last_os_error();
        match e.raw_os_error() {
            Some(libc::EINTR) => continue,
            // Not (or no longer) a child to wait for: reaped already.
            Some(libc::ECHILD) => return Ok(true),
            _ => return Err(e),
        }
    }
}

/// Sends `signal` to the process group `pgid`; a group with no member left
/// is fine.
#[cfg(unix)]
fn signal_group(pgid: u32, signal: libc::c_int) -> io::Result<()> {
    let pgid = libc::pid_t::try_from(pgid)
        .ok()
        .filter(|p| *p > 1)
        .ok_or_else(|| io::Error::other("not a usable process group id"))?;
    // SAFETY: plain FFI call; a negative pid addresses the group.
    if unsafe { libc::kill(-pgid, signal) } == 0 {
        return Ok(());
    }
    let e = io::Error::last_os_error();
    if e.raw_os_error() == Some(libc::ESRCH) {
        Ok(())
    } else {
        Err(e)
    }
}

#[cfg(windows)]
mod job {
    use std::io;

    use tokio::process::Child;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject, TerminateJobObject,
    };

    /// A Job Object that kills every process in it when it is closed.
    pub(super) struct Job(HANDLE);

    // SAFETY: a job handle may be used from any thread.
    unsafe impl Send for Job {}
    unsafe impl Sync for Job {}

    impl Job {
        /// A new job holding `child` (and so what it starts from now on; a
        /// process it started before this call is not in the job).
        pub(super) fn containing(child: &Child) -> io::Result<Job> {
            let process = child
                .raw_handle()
                .ok_or_else(|| io::Error::other("the process has ended"))?;
            // SAFETY: FFI calls with valid arguments; the handle is closed by
            // `Job::drop`, also when a later call fails.
            unsafe {
                let handle = CreateJobObjectW(std::ptr::null(), std::ptr::null());
                if handle.is_null() {
                    return Err(io::Error::last_os_error());
                }
                let job = Job(handle);
                let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                let set = SetInformationJobObject(
                    handle,
                    JobObjectExtendedLimitInformation,
                    (&raw const info).cast(),
                    size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                );
                if set == 0 || AssignProcessToJobObject(handle, process) == 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(job)
            }
        }

        pub(super) fn terminate(&self) -> io::Result<()> {
            // SAFETY: FFI call on a handle this value owns.
            if unsafe { TerminateJobObject(self.0, 1) } == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }
    }

    impl Drop for Job {
        fn drop(&mut self) {
            // SAFETY: closing the handle this value owns, once.
            unsafe { CloseHandle(self.0) };
        }
    }
}

/// For tests that start process trees: whether a process still runs.
#[cfg(all(test, unix))]
pub(crate) mod test_support {
    use std::time::{Duration, Instant};

    /// Whether `pid` still runs. A zombie does not: it has ended, only its
    /// parent has not reaped it yet (for an orphan, whatever adopted it,
    /// which reaps when it likes), nor does a process that is gone. One
    /// look answers both, so that a reaping between two looks cannot make a
    /// dead process seem alive.
    pub(crate) fn alive(pid: libc::pid_t) -> bool {
        #[cfg(target_os = "linux")]
        {
            // No file once reaped; state `Z` (or `X`) before.
            match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
                Ok(stat) => !stat
                    .rsplit(')')
                    .next()
                    .unwrap_or("")
                    .trim_start()
                    .starts_with(['Z', 'X']),
                Err(_) => false,
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            // Nothing printed once reaped; state `Z` before.
            let out = std::process::Command::new("ps")
                .args(["-o", "stat=", "-p", &pid.to_string()])
                .output()
                .expect("run ps");
            let stat = String::from_utf8_lossy(&out.stdout);
            let stat = stat.trim();
            !stat.is_empty() && !stat.starts_with('Z')
        }
    }

    /// Whether `pid` stops running within 5 s. If it does not, it is
    /// killed here, so that a failing test leaves nothing running.
    pub(crate) async fn gone_soon(pid: libc::pid_t) -> bool {
        let until = Instant::now() + Duration::from_secs(5);
        while alive(pid) {
            if Instant::now() > until {
                // SAFETY: plain FFI call; `pid` was alive just now.
                unsafe { libc::kill(pid, libc::SIGKILL) };
                return false;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        true
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::process::Stdio;
    use std::time::Duration;

    use tokio::io::{AsyncBufReadExt, BufReader};

    use super::test_support::{alive, gone_soon};
    use super::*;

    /// A shell that starts a long `sleep`, prints the sleep's pid and waits.
    fn shell_with_grandchild() -> Command {
        let mut command = Command::new("sh");
        command
            .args(["-c", "sleep 300 & echo $!; wait"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped());
        command
    }

    async fn grandchild_pid(run: &mut TreeChild) -> libc::pid_t {
        let stdout = run.child.stdout.take().unwrap();
        let mut line = String::new();
        BufReader::new(stdout).read_line(&mut line).await.unwrap();
        line.trim().parse().unwrap()
    }

    #[tokio::test]
    async fn kill_takes_the_grandchildren_too() {
        let mut run = TreeChild::spawn(&mut shell_with_grandchild())
            .await
            .unwrap();
        let grandchild = grandchild_pid(&mut run).await;
        assert!(alive(grandchild));
        run.kill().await.unwrap();
        assert!(gone_soon(grandchild).await, "the grandchild survived");
        // Killing again is harmless.
        run.kill().await.unwrap();
    }

    #[tokio::test]
    async fn dropping_kills_the_group() {
        let mut run = TreeChild::spawn(&mut shell_with_grandchild())
            .await
            .unwrap();
        let grandchild = grandchild_pid(&mut run).await;
        drop(run);
        assert!(gone_soon(grandchild).await, "the grandchild survived");
    }

    #[tokio::test]
    async fn interrupt_is_sigint_to_the_group() {
        use std::os::unix::process::ExitStatusExt;
        let mut command = Command::new("sleep");
        command.arg("300").stdin(Stdio::null());
        // A test run started in the background of a non-interactive shell
        // inherits SIGINT as ignored; the child must not.
        // SAFETY: only `signal`, which is async-signal-safe, runs after fork.
        unsafe {
            command.pre_exec(|| {
                libc::signal(libc::SIGINT, libc::SIG_DFL);
                Ok(())
            });
        }
        let mut run = TreeChild::spawn(&mut command).await.unwrap();
        run.interrupt().unwrap();
        let status = tokio::time::timeout(Duration::from_secs(5), run.child.wait())
            .await
            .expect("sleep did not stop")
            .unwrap();
        assert_eq!(status.signal(), Some(libc::SIGINT));
        // Reaped: interrupting again is a no-op.
        run.interrupt().unwrap();
    }

    /// A shell that starts a long `sleep`, prints its pid and exits at once.
    fn shell_that_leaves_a_grandchild() -> Command {
        let mut command = Command::new("sh");
        command
            .args(["-c", "sleep 300 & echo $!"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped());
        command
    }

    #[tokio::test]
    async fn a_reaped_leader_is_never_signalled_again() {
        let mut run = TreeChild::spawn(&mut shell_that_leaves_a_grandchild())
            .await
            .unwrap();
        let straggler = grandchild_pid(&mut run).await;
        // The leader exits by itself and is reaped.
        assert!(run.child.wait().await.unwrap().success());
        assert!(run.child.id().is_none());
        // Neither call may signal the (possibly reused) group: if one did,
        // the straggler, which is still in it, would be gone.
        run.interrupt().unwrap();
        run.kill().await.unwrap();
        drop(run);
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(alive(straggler), "a reaped group was signalled");
        // SAFETY: cleaning up the sleep started above.
        unsafe { libc::kill(straggler, libc::SIGKILL) };
        assert!(gone_soon(straggler).await);
    }

    #[tokio::test]
    async fn an_exited_but_unreaped_leader_still_kills_its_group() {
        let mut run = TreeChild::spawn(&mut shell_that_leaves_a_grandchild())
            .await
            .unwrap();
        let straggler = grandchild_pid(&mut run).await;
        // Let the shell exit; nobody reaps it, so its pid still names the group.
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(run.child.id().is_some());
        run.kill().await.unwrap();
        assert!(gone_soon(straggler).await, "the straggler survived");
    }

    #[tokio::test]
    async fn wait_kills_what_the_process_leaves_behind() {
        let mut run = TreeChild::spawn(&mut shell_that_leaves_a_grandchild())
            .await
            .unwrap();
        let straggler = grandchild_pid(&mut run).await;
        let status = tokio::time::timeout(Duration::from_secs(5), run.wait())
            .await
            .expect("the shell did not end")
            .unwrap();
        assert!(status.success());
        assert!(run.child.id().is_none(), "reaped");
        assert!(gone_soon(straggler).await, "the straggler survived");
        // Waiting again gives the same status.
        assert!(run.wait().await.unwrap().success());
    }

    #[tokio::test]
    async fn a_process_that_finished_is_left_alone() {
        let mut command = Command::new("true");
        let mut run = TreeChild::spawn(&mut command).await.unwrap();
        assert!(run.child.wait().await.unwrap().success());
        run.kill().await.unwrap();
    }
}
