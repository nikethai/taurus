//! What `Engine::convert` takes as a source: a file of no job, or of a job
//! that has completed, that no download is still writing (ruling R5 of
//! M6b: a path of no job stays allowed, so `taurus convert <PATH>` works).

use std::path::{Path, PathBuf};

use crate::convert::temp_folder_job;
use crate::errors::{CoreError, ErrorKind};
use crate::filename::PARTIAL_EXT;
use crate::store::{JobKind, JobState};

use super::{Engine, JobView, io_error, read};

/// Extensions of files that are still being written by a download (ours,
/// yt-dlp's, browsers').
const UNFINISHED_EXTS: [&str; 4] = [PARTIAL_EXT, "part", "ytdl", "crdownload"];

/// The refusal of a file a job is still writing, or a job that has not
/// completed.
pub(super) fn unfinished() -> CoreError {
    CoreError {
        kind: ErrorKind::Unknown,
        detail: "that file isn't finished yet".to_string(),
    }
}

impl Engine {
    /// The file as the engine will record it (absolute, no symlinks), if it
    /// is a file that no unfinished job owns and no download is writing.
    pub(super) async fn check_source(&self, path: &Path) -> Result<PathBuf, CoreError> {
        let resolved = resolve(path);
        if has_unfinished_ext(path)
            || has_unfinished_ext(&resolved)
            || self.owned_by_unfinished_job(&resolved)
        {
            return Err(unfinished());
        }
        let is_file = tokio::fs::metadata(&resolved)
            .await
            .is_ok_and(|m| m.is_file());
        if !is_file {
            return Err(CoreError {
                kind: ErrorKind::Unknown,
                detail: format!("not a file that exists: {}", path.display()),
            });
        }
        dunce::canonicalize(&resolved).map_err(io_error)
    }

    /// Whether a job that has not completed owns `path`; see [`owns`].
    fn owned_by_unfinished_job(&self, path: &Path) -> bool {
        read(&self.views)
            .values()
            .any(|v| !matches!(v.state, JobState::Completed | JobState::Seeding) && owns(v, path))
    }
}

/// `path` without symlinks: canonical if it exists, else its folder's
/// canonical path and its name (a conversion's output may not exist yet),
/// else as given.
fn resolve(path: &Path) -> PathBuf {
    if let Ok(canonical) = dunce::canonicalize(path) {
        return canonical;
    }
    let folder = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    match (dunce::canonicalize(folder), path.file_name()) {
        (Ok(folder), Some(name)) => folder.join(name),
        _ => path.to_path_buf(),
    }
}

fn has_unfinished_ext(path: &Path) -> bool {
    path.extension()
        .is_some_and(|e| UNFINISHED_EXTS.iter().any(|u| e.eq_ignore_ascii_case(u)))
}

/// Whether the job writes, or will write, `path`: its file (by its name in
/// its folder); for a torrent or a media job anything inside that name too
/// (a torrent's folder, a playlist's) and a media job's finished files; for
/// a conversion, anything in its private temporary folder.
fn owns(view: &JobView, path: &Path) -> bool {
    if view.file_name.is_empty() {
        return false;
    }
    let own = view.dest_dir.join(&view.file_name);
    match view.kind {
        JobKind::Http => path == own,
        JobKind::Torrent => path.starts_with(&own),
        JobKind::Media => {
            path.starts_with(&own)
                || view
                    .media
                    .as_ref()
                    .is_some_and(|m| m.files.iter().any(|f| f == path))
        }
        JobKind::Convert => path == own || in_temp_folder_of(view, path),
    }
}

/// Whether `path` is inside the conversion's temporary folder,
/// `.hyades-convert-<job>-<n>` in its own folder.
fn in_temp_folder_of(view: &JobView, path: &Path) -> bool {
    path.ancestors().any(|folder| {
        folder.parent() == Some(view.dest_dir.as_path())
            && folder
                .file_name()
                .and_then(|name| name.to_str())
                .and_then(temp_folder_job)
                == Some(view.id)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::JobId;

    fn view(kind: JobKind, name: &str) -> JobView {
        JobView {
            id: JobId(4),
            url: String::new(),
            source_url: String::new(),
            file_name: name.into(),
            dest_dir: PathBuf::from("/d"),
            state: JobState::Downloading,
            total: None,
            downloaded: 0,
            speed_bps: 0,
            resumable: false,
            error: None,
            kind,
            torrent: None,
            retry_at: None,
            finished_at: None,
            info_hash: None,
            media: None,
            progress_unit: Default::default(),
        }
    }

    #[test]
    fn what_a_job_owns() {
        let p = Path::new;
        let http = view(JobKind::Http, "a.bin");
        assert!(owns(&http, p("/d/a.bin")));
        assert!(!owns(&http, p("/d/a.bin2")));
        assert!(!owns(&http, p("/e/a.bin")));
        let torrent = view(JobKind::Torrent, "pack");
        assert!(owns(&torrent, p("/d/pack/sub/x.webm")));
        assert!(!owns(&torrent, p("/d/pack.webm")));
        let convert = view(JobKind::Convert, "song (mp3).mp3");
        assert!(owns(&convert, p("/d/song (mp3).mp3")));
        assert!(owns(&convert, p("/d/.hyades-convert-4-0/out.mp3")));
        // Another job's folder, or one deeper down, is not this job's.
        assert!(!owns(&convert, p("/d/.hyades-convert-5-0/out.mp3")));
        assert!(!owns(&convert, p("/d/x/.hyades-convert-4-0/out.mp3")));
        assert!(!owns(&convert, p("/d/song.webm")));
        assert!(!owns(&view(JobKind::Http, ""), p("/d")));
    }

    #[test]
    fn unfinished_extensions() {
        for name in [
            "a.mp4.part",
            "a.bin.tapa",
            "a.TAPA",
            "b.ytdl",
            "c.crdownload",
        ] {
            assert!(has_unfinished_ext(Path::new(name)), "{name}");
        }
        for name in ["a.mp4", "partial", "a.tap"] {
            assert!(!has_unfinished_ext(Path::new(name)), "{name}");
        }
    }
}
