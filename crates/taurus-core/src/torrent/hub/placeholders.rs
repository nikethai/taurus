//! The empty files librqbit makes for a torrent's unselected files.
//!
//! librqbit creates every file of a torrent when it opens the torrent's
//! storage, selected or not. Before it does, the hub records which
//! unselected files are not on disk; those that are still empty afterwards
//! are deleted (with the folders that leaves empty, up to the job's save
//! folder), while librqbit has the torrent and again once it lets go of it.
//! A file that was there before, or that holds any byte, is never deleted.
//!
//! A deleted file whose torrent is still in the session is one librqbit
//! still has open: selected later, it would be written where nobody can
//! see it. Such a torrent is dropped from the session first (files kept),
//! so the next add opens, makes and checks its files again.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

use super::{TorrentHub, delete_in, lock};
use crate::errors::CoreError;
use crate::store::JobRecord;
use crate::torrent::TorrentInfo;
use crate::torrent::files::{file_path, is_selected, stored_output_dir};
use crate::torrent::hub::info_from_bytes;

/// The placeholders of every torrent, by info-hash.
pub(super) type Placeholders = HashMap<String, Recorded>;

/// One torrent's placeholders: the files (by index) that were not on disk
/// before librqbit opened them, and the job's save folder.
#[derive(Debug, Default)]
pub(super) struct Recorded {
    save: PathBuf,
    files: BTreeMap<usize, PathBuf>,
}

impl TorrentHub {
    /// Before librqbit opens the torrent's files (into `output`, inside the
    /// job's save folder `save`), with `only_files` selected: records each
    /// unselected file that is not on disk, and forgets the others (selected
    /// ones, and any that is on disk now, whoever made it).
    pub fn note_placeholders(
        &self,
        save: &Path,
        output: &Path,
        info: &TorrentInfo,
        only_files: Option<&[usize]>,
    ) {
        let mut all = lock(&self.placeholders);
        let recorded = all.entry(info.info_hash.clone()).or_default();
        note(recorded, save, output, info, only_files);
        if recorded.files.is_empty() {
            all.remove(&info.info_hash);
        }
    }

    /// [`TorrentHub::note_placeholders`] for each stored job whose torrent
    /// the session about to start may open.
    pub(super) fn note_jobs(&self, jobs: &[&JobRecord]) {
        for job in jobs {
            let Some(info) = job
                .torrent_meta
                .as_deref()
                .and_then(|m| info_from_bytes(m).ok())
            else {
                continue;
            };
            let output = stored_output_dir(&job.dest_dir, &job.file_name, &info);
            self.note_placeholders(&job.dest_dir, &output, &info, job.only_files.as_deref());
        }
    }

    /// Deletes the torrent's recorded files that are still empty, and the
    /// folders that leaves empty up to (not including) the save folder. A
    /// recorded file that has bytes now is never deleted, nor recorded any
    /// longer; one that is not on disk stays recorded (librqbit makes it
    /// again if it opens the files again).
    pub fn clean_placeholders(&self, info_hash: &str) {
        if let Some(recorded) = lock(&self.placeholders).get_mut(info_hash) {
            clean(recorded);
        }
    }

    /// Forgets the torrent's placeholders (its job is removed).
    pub fn forget_placeholders(&self, info_hash: &str) {
        lock(&self.placeholders).remove(info_hash);
    }

    /// Drops the torrent from the session (files kept) if `only_files`
    /// selects a recorded file that is gone from disk while the session has
    /// the torrent: librqbit holds that file open and would write it where
    /// nobody sees it. The next add opens the files again.
    pub async fn reopen_deleted(
        &self,
        info: &TorrentInfo,
        only_files: Option<&[usize]>,
    ) -> Result<(), CoreError> {
        let stale = lock(&self.placeholders)
            .get(&info.info_hash)
            .is_some_and(|r| {
                r.files
                    .iter()
                    .any(|(&i, path)| is_selected(only_files, i) && !exists(path))
            });
        match self.current() {
            Some(session) if stale && self.handle(&info.info_hash).is_some() => {
                tracing::info!(info_hash = %info.info_hash, "opening the torrent's files again");
                delete_in(&session, &info.info_hash, false).await
            }
            _ => Ok(()),
        }
    }
}

fn note(
    recorded: &mut Recorded,
    save: &Path,
    output: &Path,
    info: &TorrentInfo,
    only_files: Option<&[usize]>,
) {
    recorded.save = save.to_path_buf();
    for file in &info.files {
        let path = file_path(output, file);
        // Selected, or on disk before librqbit opens the files (whoever
        // made it): never a placeholder of this run.
        if is_selected(only_files, file.index) || exists(&path) {
            recorded.files.remove(&file.index);
        } else {
            recorded.files.insert(file.index, path);
        }
    }
}

fn clean(recorded: &mut Recorded) {
    let Recorded { save, files } = recorded;
    files.retain(|_, path| match fs::symlink_metadata(&*path) {
        Ok(meta) if meta.is_file() && meta.len() == 0 => {
            match fs::remove_file(&*path) {
                Ok(()) => remove_empty_folders(path, save),
                Err(e) => tracing::debug!("could not delete {}: {e}", path.display()),
            }
            true
        }
        // Holds bytes, or is no plain file: not a placeholder.
        Ok(_) => false,
        Err(_) => true,
    });
}

/// Removes the empty folders above `file`, up to (not including) `save`.
/// Stops at the first one that is not a real folder (a link, or on Windows
/// a junction), or that is not empty.
fn remove_empty_folders(file: &Path, save: &Path) {
    let mut folder = file.parent();
    while let Some(dir) = folder {
        if dir == save || !dir.starts_with(save) || !is_real_dir(dir) {
            break;
        }
        if fs::remove_dir(dir).is_err() {
            break;
        }
        folder = dir.parent();
    }
}

/// Whether `dir` is a folder itself, not a link (or reparse point) to one.
fn is_real_dir(dir: &Path) -> bool {
    let Ok(meta) = fs::symlink_metadata(dir) else {
        return false;
    };
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return false;
        }
    }
    meta.is_dir() && !meta.file_type().is_symlink()
}

/// Whether anything is at `path` (a dangling link counts).
fn exists(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::torrent::TorrentFile;

    fn info(paths: &[&str]) -> TorrentInfo {
        let files: Vec<TorrentFile> = paths
            .iter()
            .enumerate()
            .map(|(index, path)| TorrentFile {
                index,
                path: (*path).to_string(),
                size: 10,
            })
            .collect();
        TorrentInfo {
            info_hash: "0".repeat(40),
            name: "t".into(),
            total: 10 * files.len() as u64,
            files,
        }
    }

    /// Makes every file of `info` in `output` as librqbit does: empty.
    fn open_all(output: &Path, info: &TorrentInfo) {
        for file in &info.files {
            let path = file_path(output, file);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .unwrap();
        }
    }

    #[test]
    fn deletes_only_new_empty_unselected_files() {
        let dir = tempfile::tempdir().unwrap();
        let save = dir.path().join("save");
        let output = save.join("t");
        let t = info(&["a/x", "b", "c/d/y", "e/z", "f"]);
        // Already there: "f" (empty) and "e/z" (with bytes).
        fs::create_dir_all(output.join("e")).unwrap();
        fs::write(output.join("f"), b"").unwrap();
        fs::write(output.join("e/z"), b"mine").unwrap();
        let mut recorded = Recorded::default();
        note(&mut recorded, &save, &output, &t, Some(&[1]));
        assert_eq!(recorded.files.keys().copied().collect::<Vec<_>>(), [0, 2]);

        open_all(&output, &t);
        // A piece shared with a selected file wrote into "a/x".
        fs::write(output.join("a/x"), b"tail").unwrap();
        clean(&mut recorded);
        assert_eq!(fs::read(output.join("a/x")).unwrap(), b"tail");
        assert!(output.join("b").exists());
        assert!(!output.join("c").exists(), "an empty folder is left");
        assert_eq!(fs::read(output.join("e/z")).unwrap(), b"mine");
        assert!(output.join("f").exists());
        // "a/x" has bytes: it is not recorded any more; "c/d/y" stays
        // recorded in case librqbit makes it again.
        assert_eq!(recorded.files.keys().copied().collect::<Vec<_>>(), [2]);

        // Selected now: forgotten, never deleted.
        note(&mut recorded, &save, &output, &t, Some(&[1, 2]));
        assert!(recorded.files.is_empty());
    }

    /// A recorded file that is gone, then made by someone else, is theirs:
    /// the next run's note forgets it, and it is kept.
    #[test]
    fn file_made_later_by_someone_else_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let save = dir.path().join("save");
        let output = save.join("t");
        let t = info(&["a/x", "b"]);
        let mut recorded = Recorded::default();
        note(&mut recorded, &save, &output, &t, Some(&[1]));
        open_all(&output, &t);
        clean(&mut recorded);
        assert!(!output.join("a").exists());
        assert_eq!(recorded.files.len(), 1, "a missing file stays recorded");

        fs::create_dir_all(output.join("a")).unwrap();
        fs::write(output.join("a/x"), b"").unwrap();
        // The next run.
        note(&mut recorded, &save, &output, &t, Some(&[1]));
        assert!(recorded.files.is_empty());
        clean(&mut recorded);
        assert_eq!(fs::metadata(output.join("a/x")).unwrap().len(), 0);
    }

    /// The folder walk stops at a link: neither it nor what it points to
    /// is removed.
    #[cfg(unix)]
    #[test]
    fn stops_at_a_linked_folder() {
        let dir = tempfile::tempdir().unwrap();
        let save = dir.path().join("save");
        let output = save.join("t");
        let elsewhere = dir.path().join("elsewhere");
        fs::create_dir_all(elsewhere.join("in")).unwrap();
        fs::create_dir_all(&output).unwrap();
        std::os::unix::fs::symlink(&elsewhere, output.join("l")).unwrap();
        let t = info(&["l/in/x", "b"]);
        let mut recorded = Recorded::default();
        note(&mut recorded, &save, &output, &t, Some(&[1]));
        open_all(&output, &t);
        clean(&mut recorded);
        assert!(!elsewhere.join("in/x").exists());
        // Through the link, "in" is a real, empty folder: it goes; the link
        // and its target stay.
        assert!(!elsewhere.join("in").exists());
        assert!(fs::symlink_metadata(output.join("l")).unwrap().is_symlink());
        assert!(elsewhere.is_dir());
        assert!(output.is_dir());
    }

    #[test]
    fn keeps_the_save_folder() {
        let dir = tempfile::tempdir().unwrap();
        let save = dir.path().join("save");
        let t = info(&["sub/x", "y"]);
        let mut recorded = Recorded::default();
        // Saved straight into the save folder.
        note(&mut recorded, &save, &save, &t, Some(&[1]));
        open_all(&save, &t);
        fs::remove_file(save.join("y")).unwrap();
        clean(&mut recorded);
        assert!(!save.join("sub").exists());
        assert!(save.is_dir(), "the save folder went");
    }
}
