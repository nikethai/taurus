//! Torrent file-path safety and where a torrent's files are saved.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use super::{TorrentFile, TorrentInfo};
use crate::errors::{CoreError, ErrorKind};
use crate::filename::{MAX_CANDIDATE_BYTES, is_taken, sanitize};

const RESERVED_STEMS: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// Rejects a torrent whose file paths could escape the save folder or are
/// not valid names on this OS.
pub fn check_paths(files: &[TorrentFile]) -> Result<(), CoreError> {
    check_paths_for(files, cfg!(windows))
}

/// [`check_paths`] with the Windows rules chosen explicitly, so every OS can
/// test them.
fn check_paths_for(files: &[TorrentFile], windows: bool) -> Result<(), CoreError> {
    for file in files {
        if let Some(reason) = unsafe_reason(&file.path, windows) {
            return Err(CoreError {
                kind: ErrorKind::UnsafeTorrent,
                detail: format!("unsafe file path {:?}: {reason}", file.path),
            });
        }
    }
    Ok(())
}

fn unsafe_reason(path: &str, windows: bool) -> Option<&'static str> {
    if path.is_empty() {
        return Some("empty path");
    }
    for (i, part) in path.split('/').enumerate() {
        match part {
            "" => return Some("empty or rooted path component"),
            "." | ".." => return Some("relative path component"),
            _ => {}
        }
        if part.contains('\\') {
            return Some("path separator in a name");
        }
        if part.chars().any(|c| c.is_ascii_control()) {
            return Some("control character in a name");
        }
        let b = part.as_bytes();
        if i == 0 && b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
            return Some("drive prefix");
        }
        if windows {
            if part.contains(['<', '>', ':', '"', '|', '?', '*']) {
                return Some("character not allowed on Windows");
            }
            // Win32 strips these, so `CON ` or `a.` alias other names.
            if part.ends_with(['.', ' ']) {
                return Some("name ends with a dot or space");
            }
            let stem = part.split('.').next().unwrap_or("");
            if RESERVED_STEMS.iter().any(|r| r.eq_ignore_ascii_case(stem)) {
                return Some("reserved name on Windows");
            }
        }
    }
    None
}

/// Where the torrent's files go. A multi-file torrent gets its own folder
/// `dest/<name>` (suffixed ` (n)` if taken). A single-file one is saved
/// straight into `dest` unless what librqbit would write there (its raw,
/// unsanitized path, whose first component is `dest/<file>` or, for a nested
/// path, `dest/<dir>`) is taken; then it goes into `dest/<name> (n)` so two
/// torrents never mix. `reserved` holds names other jobs have claimed in
/// `dest`.
pub fn output_dir(dest: &Path, info: &TorrentInfo, reserved: &HashSet<String>) -> PathBuf {
    let taken = |name: &str| is_taken(dest, name, reserved);

    if let [file] = info.files.as_slice() {
        let top = file.path.split('/').next().unwrap_or("");
        let safe_top = sanitize(top);
        if !taken(top) && !taken(&safe_top) {
            return dest.to_path_buf();
        }
        // A nested path keeps its directory name whole; a bare file name
        // loses its extension.
        let base = if file.path.contains('/') {
            safe_top.as_str()
        } else {
            Path::new(&safe_top)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or(&safe_top)
        };
        return first_free(dest, base, 1, &taken);
    }
    first_free(dest, &sanitize(&info.name), 0, &taken)
}

/// `dest/<base>` (only when `first` is 0), then `dest/<base> (n)` from
/// `first.max(1)`; `base` is cut so `<name>.tapa` stays within 255 bytes.
/// Folder names keep any dots (`Show.S01.1080p`), unlike file names.
fn first_free(dest: &Path, base: &str, first: u32, taken: &dyn Fn(&str) -> bool) -> PathBuf {
    let candidate = |n: u32| {
        let suffix = if n == 0 {
            String::new()
        } else {
            format!(" ({n})")
        };
        let mut end = base.len().min(MAX_CANDIDATE_BYTES - suffix.len());
        while !base.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}{suffix}", &base[..end])
    };
    (first..)
        .map(candidate)
        .find(|name| !taken(name))
        .map(|name| dest.join(name))
        .expect("unbounded counter yields a free name")
}

/// What a torrent job stores as its `file_name`: the entry its files occupy
/// in `dest`. That is the raw first path component of a single file saved
/// straight into `dest` (the name librqbit writes), otherwise the name of the
/// folder [`output_dir`] chose (`Show (1)`).
pub fn stored_name(dest: &Path, output: &Path, info: &TorrentInfo) -> String {
    match info.files.as_slice() {
        [file] if output == dest => file.path.split('/').next().unwrap_or("").to_string(),
        _ => output
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
    }
}

/// Where a stored torrent job's files are: the inverse of [`stored_name`].
/// A folder chosen for a single file always has a ` (n)` suffix, so it never
/// equals the file's own first path component.
pub fn stored_output_dir(dest: &Path, file_name: &str, info: &TorrentInfo) -> PathBuf {
    match info.files.as_slice() {
        [file] if file.path.split('/').next() == Some(file_name) => dest.to_path_buf(),
        _ => dest.join(file_name),
    }
}

/// Bytes of the selected files (`None` selects every file).
pub fn selected_total(info: &TorrentInfo, only_files: Option<&[usize]>) -> u64 {
    info.files
        .iter()
        .filter(|f| only_files.is_none_or(|only| only.contains(&f.index)))
        .fold(0u64, |sum, f| sum.saturating_add(f.size))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn file(index: usize, path: &str, size: u64) -> TorrentFile {
        TorrentFile {
            index,
            path: path.to_string(),
            size,
        }
    }

    fn info(name: &str, files: Vec<TorrentFile>) -> TorrentInfo {
        let total = files.iter().map(|f| f.size).sum();
        TorrentInfo {
            info_hash: "0".repeat(40),
            name: name.to_string(),
            files,
            total,
        }
    }

    fn unsafe_path(path: &str, windows: bool) -> bool {
        let e = check_paths_for(&[file(0, path, 1)], windows).err();
        e.is_some_and(|e| e.kind == ErrorKind::UnsafeTorrent)
    }

    #[test]
    fn rejects_parent_and_absolute() {
        for path in [
            "../a", "a/../b", "/etc/x", "a//b", "a/", "", ".", "a/./b", "\\x", "a\\b", "C:",
            "C:/x", "c:x", "a\u{0}b", "a/b\nc", "\u{7f}x",
        ] {
            assert!(unsafe_path(path, false), "{path:?}");
            assert!(check_paths(&[file(0, path, 1)]).is_err(), "{path:?}");
        }
        for path in ["a", "a/b.txt", "Show/S01/e1.mkv", "..a", "a..", "x:/y/c"] {
            // "x:/y/c" has a drive-like first component only if it is `x:`.
            let expect = path.starts_with("x:");
            assert_eq!(unsafe_path(path, false), expect, "{path:?}");
        }
        assert!(check_paths(&[file(0, "ok/a", 1), file(1, "ok/../b", 1)]).is_err());
        assert!(check_paths(&[]).is_ok());
    }

    #[test]
    fn rejects_reserved_windows_names() {
        for path in [
            "CON.txt",
            "a/nul",
            "a/Com1.x/b",
            "a:b",
            "a/b?c",
            "a/b*",
            "d\"e",
            "CON ",
            "a/b.",
            "a/b ",
        ] {
            assert!(unsafe_path(path, true), "{path:?}");
            if path != "a:b" && !path.contains(['?', '*', '"']) && !path.ends_with(['.', ' ']) {
                assert!(!unsafe_path(path, false), "{path:?} is fine off Windows");
            }
        }
        assert!(!unsafe_path("console/a.txt", true));
        assert!(!unsafe_path("a/b.txt", true));
        #[cfg(windows)]
        assert!(check_paths(&[file(0, "CON.txt", 1)]).is_err());
    }

    #[test]
    fn multi_file_folder_suffix() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        let multi = info("Show", vec![file(0, "Show/a", 1), file(1, "Show/b", 2)]);
        let none = HashSet::new();
        assert_eq!(output_dir(d, &multi, &none), d.join("Show"));
        fs::create_dir(d.join("Show")).unwrap();
        assert_eq!(output_dir(d, &multi, &none), d.join("Show (1)"));
        let reserved: HashSet<String> = ["show (1)".to_string()].into();
        assert_eq!(output_dir(d, &multi, &reserved), d.join("Show (2)"));
    }

    #[test]
    fn multi_file_folder_keeps_dots() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        let multi = info("Show.S01.1080p", vec![file(0, "a", 1), file(1, "b", 2)]);
        fs::create_dir(d.join("Show.S01.1080p")).unwrap();
        assert_eq!(
            output_dir(d, &multi, &HashSet::new()),
            d.join("Show.S01.1080p (1)")
        );
    }

    #[test]
    fn multi_file_name_is_sanitized() {
        let dir = tempfile::tempdir().unwrap();
        let multi = info("../evil", vec![file(0, "a", 1), file(1, "b", 2)]);
        assert_eq!(
            output_dir(dir.path(), &multi, &HashSet::new()),
            dir.path().join("evil")
        );
    }

    #[test]
    fn single_file_free_goes_to_dest() {
        let dir = tempfile::tempdir().unwrap();
        let single = info("movie.mkv", vec![file(0, "movie.mkv", 5)]);
        assert_eq!(output_dir(dir.path(), &single, &HashSet::new()), dir.path());
    }

    #[test]
    fn single_file_taken_gets_folder() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        let single = info("movie.mkv", vec![file(0, "movie.mkv", 5)]);
        let none = HashSet::new();
        fs::write(d.join("movie.mkv"), b"x").unwrap();
        assert_eq!(output_dir(d, &single, &none), d.join("movie (1)"));
        fs::create_dir(d.join("movie (1)")).unwrap();
        assert_eq!(output_dir(d, &single, &none), d.join("movie (2)"));

        // A partial file or a reservation (any case) also counts as taken.
        let d2 = tempfile::tempdir().unwrap();
        fs::write(d2.path().join("movie.mkv.tapa"), b"x").unwrap();
        assert_eq!(
            output_dir(d2.path(), &single, &none),
            d2.path().join("movie (1)")
        );
        let d3 = tempfile::tempdir().unwrap();
        let reserved: HashSet<String> = ["Movie.MKV".to_string()].into();
        assert_eq!(
            output_dir(d3.path(), &single, &reserved),
            d3.path().join("movie (1)")
        );
    }

    // Where librqbit writes the raw name, not the sanitized one.
    #[cfg(unix)]
    #[test]
    fn single_file_checks_the_raw_name() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        let single = info("Movie: x.mkv", vec![file(0, "Movie: x.mkv", 5)]);
        assert_eq!(output_dir(d, &single, &HashSet::new()), d);
        fs::write(d.join("Movie: x.mkv"), b"x").unwrap();
        assert_eq!(
            output_dir(d, &single, &HashSet::new()),
            d.join("Movie_ x (1)")
        );
    }

    #[test]
    fn single_file_nested_path_checks_its_top_directory() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        let single = info("sub", vec![file(0, "sub/x.mkv", 5)]);
        assert_eq!(output_dir(d, &single, &HashSet::new()), d);
        fs::create_dir(d.join("sub")).unwrap();
        fs::write(d.join("sub/x.mkv"), b"x").unwrap();
        assert_eq!(output_dir(d, &single, &HashSet::new()), d.join("sub (1)"));
        let reserved: HashSet<String> = ["SUB".to_string()].into();
        let other = tempfile::tempdir().unwrap();
        assert_eq!(
            output_dir(other.path(), &single, &reserved),
            other.path().join("sub (1)")
        );
    }

    #[test]
    fn long_folder_names_fit_with_partial_suffix() {
        let dir = tempfile::tempdir().unwrap();
        let multi = info(&"é".repeat(200), vec![file(0, "a", 1), file(1, "b", 2)]);
        fs::create_dir(output_dir(dir.path(), &multi, &HashSet::new())).unwrap();
        let second = output_dir(dir.path(), &multi, &HashSet::new());
        let name = second.file_name().unwrap().to_str().unwrap();
        assert!(name.len() + 5 <= 255, "len {}", name.len());
        assert!(name.ends_with(" (1)"));
    }

    #[test]
    fn stored_name_roundtrips_output_dir() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        let none = HashSet::new();
        let single = info("movie.mkv", vec![file(0, "movie.mkv", 5)]);
        let nested = info("sub", vec![file(0, "sub/x.mkv", 5)]);
        let multi = info("Show", vec![file(0, "a", 1), file(1, "b", 2)]);
        let mut seen = Vec::new();
        for _ in 0..2 {
            for t in [&single, &nested, &multi] {
                let out = output_dir(d, t, &none);
                let name = stored_name(d, &out, t);
                assert_eq!(stored_output_dir(d, &name, t), out, "{name}");
                seen.push(name);
            }
            // Take every name, so the second round gets folders.
            fs::write(d.join("movie.mkv"), b"x").unwrap();
            fs::create_dir_all(d.join("sub")).unwrap();
            fs::create_dir_all(d.join("Show")).unwrap();
        }
        assert_eq!(
            seen,
            [
                "movie.mkv",
                "sub",
                "Show",
                "movie (1)",
                "sub (1)",
                "Show (1)"
            ]
        );
    }

    #[test]
    fn selected_total_counts_selection() {
        let t = info(
            "t",
            vec![file(0, "a", 10), file(1, "b", 20), file(2, "c", 30)],
        );
        assert_eq!(selected_total(&t, None), 60);
        assert_eq!(selected_total(&t, Some(&[0, 2])), 40);
        assert_eq!(selected_total(&t, Some(&[])), 0);
        assert_eq!(selected_total(&t, Some(&[7])), 0);
    }
}
