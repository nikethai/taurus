//! A media job's own partial files, for removing it: those its runs
//! recorded (yt-dlp's `tmpfilename` and `filename`, see
//! `run::Tracker::partials`) and the files yt-dlp derives from exactly those
//! names. Nothing is found by a video id or any other name pattern, so
//! another job's files, and the user's, stay.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// The files of `recorded` that exist, with yt-dlp's companions of each
/// recorded name `<n>`: `<n>.ytdl` (for `<f>.part` also `<f>.ytdl`), the
/// fragments `<n>-Frag<N>` and `<n>.part-Frag<N>` (each possibly with its
/// own `.part`), and the postprocessors' `<stem>.temp.<ext>`. None of
/// `keep` (the job's finished files) is ever returned.
pub(crate) fn own_partial_files(recorded: &[PathBuf], keep: &[PathBuf]) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = Vec::new();
    let mut add = |path: PathBuf| {
        if !keep.contains(&path) && !found.contains(&path) && path.is_file() {
            found.push(path);
        }
    };
    // The recorded names in each folder, for the fragments.
    let mut by_dir: BTreeMap<&Path, Vec<OsString>> = BTreeMap::new();
    for path in recorded {
        let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else {
            continue;
        };
        add(path.clone());
        add(dir.join(with_suffix(name, ".ytdl")));
        let text = name.to_string_lossy();
        if let Some(file) = text.strip_suffix(".part") {
            add(dir.join(format!("{file}.ytdl")));
        } else if let Some((stem, ext)) = text.rsplit_once('.')
            && !stem.is_empty()
        {
            add(dir.join(format!("{stem}.temp.{ext}")));
        }
        by_dir.entry(dir).or_default().push(name.to_os_string());
    }
    for (dir, names) in by_dir {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let file = entry.file_name();
            if names.iter().any(|name| is_fragment(name, &file)) {
                add(entry.path());
            }
        }
    }
    found
}

fn with_suffix(name: &std::ffi::OsStr, suffix: &str) -> OsString {
    let mut name = name.to_os_string();
    name.push(suffix);
    name
}

/// Whether `file` is a fragment of the recorded `name`: `<name>-Frag<N>`
/// or `<name>.part-Frag<N>`, optionally followed by `.part`.
fn is_fragment(name: &std::ffi::OsStr, file: &std::ffi::OsStr) -> bool {
    let (name, file) = (name.to_string_lossy(), file.to_string_lossy());
    let Some(rest) = file.strip_prefix(name.as_ref()) else {
        return false;
    };
    let rest = rest.strip_prefix(".part").unwrap_or(rest);
    let Some(number) = rest.strip_prefix("-Frag") else {
        return false;
    };
    let number = number.strip_suffix(".part").unwrap_or(number);
    !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_recorded_files_and_their_companions() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let write = |name: &str| {
            let path = dir.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, b"x").unwrap();
            path
        };
        let own = [
            write("V [a].f137.mp4.part"),
            write("V [a].f137.mp4"),
            write("V [a].f137.mp4.ytdl"),
            write("V [a].f137.mp4.part.ytdl"),
            write("V [a].f137.mp4.part-Frag3"),
            write("V [a].f137.mp4.part-Frag12.part"),
            write("V [a].f137.temp.mp4"),
        ];
        let others = [
            // The same video's, not recorded: another job's or the user's.
            write("V [a].f140.m4a.part"),
            write("V [a].mp4"),
            write("Inner/V [a].f137.mp4.part"),
            write("V [a].f137.mp4.part-Fragment"),
            write("V [a].f137.mp4.part-Frag"),
            write("Notes [a].txt"),
        ];
        let recorded = [
            dir.join("V [a].f137.mp4.part"),
            dir.join("V [a].f137.mp4"),
            // Gone, or in a folder that is gone: nothing.
            dir.join("Gone [b].mp4.part"),
            dir.join("No/such.part"),
        ];
        let mut found = own_partial_files(&recorded, &[]);
        found.sort();
        let mut want = own.to_vec();
        want.sort();
        assert_eq!(found, want);
        for other in &others {
            assert!(!found.contains(other), "{}", other.display());
        }
        // A finished file is never one, recorded or not.
        let kept = own_partial_files(&recorded, &[dir.join("V [a].f137.mp4")]);
        assert!(!kept.contains(&dir.join("V [a].f137.mp4")));
        assert_eq!(kept.len(), own.len() - 1);
        assert!(own_partial_files(&[], &[]).is_empty());
    }
}
