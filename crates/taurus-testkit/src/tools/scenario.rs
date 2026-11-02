//! Race-free scenarios for the fake yt-dlp in tests that share a process.
//!
//! The fake reads its scenario from `FAKE_YTDLP_SCENARIO` and logs its
//! calls to `FAKE_TOOL_ARGS_LOG`, and a child inherits both from the test
//! process, where they are global to every test of the binary. So they are
//! set once, to values every test can share: the scenario path contains
//! `{version}`, which the fake replaces with its own version. A test
//! publishes yt-dlp under a version of its own ([`unique_version`]) and
//! writes that version's scenario ([`set_ytdlp_scenario`]); the args log
//! holds every test's calls, so a test picks its own out by an argument it
//! made unique (such as the link).

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, Ordering};

use super::{ArgsLogEntry, read_args_log};

/// The directory of this process's scenario files and args log.
fn scenario_dir() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let dir =
            std::env::temp_dir().join(format!("hyades-ytdlp-scenarios-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create the scenario directory");
        let scenario = dir.join("scenario-{version}.json");
        // SAFETY: this runs once, before the calling test starts any child
        // process. std serializes `set_var` with its own environment reads
        // and with `Command::spawn`; the values never change afterwards.
        unsafe {
            std::env::set_var("FAKE_YTDLP_SCENARIO", scenario);
            std::env::set_var("FAKE_TOOL_ARGS_LOG", dir.join("args.log"));
        }
        dir
    })
}

/// A yt-dlp version no other call in this process returned, such as
/// `2026.01.17`: publish it with `ToolRelease::new`, then give it a
/// scenario with [`set_ytdlp_scenario`].
pub fn unique_version() -> String {
    static NEXT: AtomicU32 = AtomicU32::new(1);
    format!("2026.01.{}", NEXT.fetch_add(1, Ordering::Relaxed))
}

/// Makes a fake yt-dlp of `version` replay `scenario_json` (the scenario
/// schema of the fake: one object, or an array with `if_arg`). Call it
/// before the engine or tool manager under test starts a yt-dlp; it also
/// points the fakes at the process-wide args log ([`process_args_log`]).
pub fn set_ytdlp_scenario(version: &str, scenario_json: &str) {
    let path = scenario_dir().join(format!("scenario-{version}.json"));
    std::fs::write(&path, scenario_json).expect("write the scenario");
}

/// Every call any fake tool made in this process, oldest first. Version
/// checks are in it; filter by tool and by arguments.
pub fn process_args_log() -> Vec<ArgsLogEntry> {
    read_args_log(&scenario_dir().join("args.log"))
}
