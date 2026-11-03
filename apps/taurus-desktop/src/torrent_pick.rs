//! The torrent file picker (pure), shared by the Advanced Add screen and
//! "Choose files…" of a torrent in the list: which files are selected,
//! grouped by the top folder of their path with tri-state folder boxes.

use taurus_core::TorrentFileProgress;
use taurus_core::torrent::TorrentInfo;

use crate::{TorrentFileVm, format};

/// One file of a torrent in the picker.
#[derive(Clone, Debug, PartialEq)]
struct PickFile {
    index: usize,
    path: String,
    size: u64,
    /// Bytes already downloaded (a torrent in the list).
    done: u64,
    selected: bool,
}

/// A folder's box: all, some or none of its files selected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GroupState {
    All,
    Some,
    None,
}

impl GroupState {
    fn as_str(self) -> &'static str {
        match self {
            GroupState::All => "all",
            GroupState::Some => "some",
            GroupState::None => "none",
        }
    }
}

/// The torrent file picker: which files are selected, grouped by the
/// top folder of their path.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Picker {
    /// In index order.
    files: Vec<PickFile>,
}

/// The top folder of a path inside a torrent; `None` for a file at the top.
fn top_folder(path: &str) -> Option<&str> {
    path.split_once('/').map(|(top, _)| top)
}

impl Picker {
    /// Every file of a torrent about to be added, all selected.
    pub(crate) fn from_info(info: &TorrentInfo) -> Picker {
        let mut files: Vec<PickFile> = info
            .files
            .iter()
            .map(|f| PickFile {
                index: f.index,
                path: f.path.clone(),
                size: f.size,
                done: 0,
                selected: true,
            })
            .collect();
        files.sort_by_key(|f| f.index);
        Picker { files }
    }

    /// The files of a torrent in the list, selected as they are now.
    pub(crate) fn from_progress(files: &[TorrentFileProgress]) -> Picker {
        let mut files: Vec<PickFile> = files
            .iter()
            .map(|f| PickFile {
                index: f.index,
                path: f.path.clone(),
                size: f.size,
                done: f.done,
                selected: f.selected,
            })
            .collect();
        files.sort_by_key(|f| f.index);
        Picker { files }
    }

    pub(crate) fn toggle_file(&mut self, index: usize) {
        if let Some(f) = self.files.iter_mut().find(|f| f.index == index) {
            f.selected = !f.selected;
        }
    }

    pub(crate) fn toggle_all(&mut self, on: bool) {
        for f in &mut self.files {
            f.selected = on;
        }
    }

    /// A folder's box: all of its files off when all are on, else all on.
    pub(crate) fn toggle_group(&mut self, folder: &str) {
        let on = self.group_state(folder) != GroupState::All;
        for f in &mut self.files {
            if top_folder(&f.path) == Some(folder) {
                f.selected = on;
            }
        }
    }

    pub(crate) fn group_state(&self, folder: &str) -> GroupState {
        let (mut on, mut off) = (0, 0);
        for f in self
            .files
            .iter()
            .filter(|f| top_folder(&f.path) == Some(folder))
        {
            if f.selected {
                on += 1;
            } else {
                off += 1;
            }
        }
        match (on, off) {
            (0, _) => GroupState::None,
            (_, 0) => GroupState::All,
            _ => GroupState::Some,
        }
    }

    /// The indices of the selected files, in order.
    pub(crate) fn chosen(&self) -> Vec<usize> {
        self.selected().map(|f| f.index).collect()
    }

    fn selected(&self) -> impl Iterator<Item = &PickFile> {
        self.files.iter().filter(|f| f.selected)
    }

    pub(crate) fn all_selected(&self) -> bool {
        self.files.iter().all(|f| f.selected)
    }

    pub(crate) fn selected_count(&self) -> usize {
        self.selected().count()
    }

    pub(crate) fn selected_size(&self) -> u64 {
        self.selected().map(|f| f.size).sum()
    }

    /// What the selected files still need on disk.
    pub(crate) fn needed(&self) -> u64 {
        self.selected().map(|f| f.size.saturating_sub(f.done)).sum()
    }

    /// The rows: files sorted by path, each top folder's files under a
    /// row of its own (the folder's tri-state box).
    pub(crate) fn rows(&self) -> Vec<TorrentFileVm> {
        let mut sorted: Vec<&PickFile> = self.files.iter().collect();
        sorted.sort_by(|a, b| a.path.cmp(&b.path));
        let mut rows = Vec::with_capacity(sorted.len() + 4);
        let mut group: Option<&str> = None;
        for f in sorted {
            let top = top_folder(&f.path);
            if let Some(folder) = top.filter(|t| group != Some(*t)) {
                let size: u64 = self
                    .files
                    .iter()
                    .filter(|g| top_folder(&g.path) == Some(folder))
                    .map(|g| g.size)
                    .sum();
                let state = self.group_state(folder);
                rows.push(TorrentFileVm {
                    index: -1,
                    name: folder.into(),
                    path: folder.into(),
                    size_text: format::bytes(size).into(),
                    selected: state == GroupState::All,
                    group: true,
                    state: state.as_str().into(),
                    in_group: false,
                });
            }
            group = top;
            let name = match top {
                Some(t) => &f.path[t.len() + 1..],
                None => f.path.as_str(),
            };
            rows.push(TorrentFileVm {
                index: i32::try_from(f.index).unwrap_or(i32::MAX),
                name: name.into(),
                path: f.path.as_str().into(),
                size_text: format::bytes(f.size).into(),
                selected: f.selected,
                group: false,
                state: if f.selected { "all" } else { "none" }.into(),
                in_group: top.is_some(),
            });
        }
        rows
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use taurus_core::torrent::TorrentFile;

    fn torrent() -> TorrentInfo {
        let file = |index, path: &str, size| TorrentFile {
            index,
            path: path.into(),
            size,
        };
        TorrentInfo {
            info_hash: "0".repeat(40),
            name: "pack".into(),
            // Index order is not path order.
            files: vec![
                file(0, "b.bin", 300),
                file(1, "a/2.bin", 200),
                file(2, "a/1.bin", 100),
            ],
            total: 600,
        }
    }

    fn shown(p: &Picker) -> Vec<(String, i32, String)> {
        p.rows()
            .into_iter()
            .map(|r| (r.path.to_string(), r.index, r.state.to_string()))
            .collect()
    }

    fn row(path: &str, index: i32, state: &str) -> (String, i32, String) {
        (path.into(), index, state.into())
    }

    #[test]
    fn picker_groups_by_top_folder() {
        let mut p = Picker::from_info(&torrent());
        assert_eq!(
            shown(&p),
            [
                row("a", -1, "all"),
                row("a/1.bin", 2, "all"),
                row("a/2.bin", 1, "all"),
                row("b.bin", 0, "all"),
            ]
        );
        let rows = p.rows();
        assert_eq!(
            (rows[0].size_text.as_str(), rows[1].name.as_str()),
            ("300 B", "1.bin")
        );
        assert!(rows[1].in_group && !rows[3].in_group);
        assert!(p.all_selected());
        p.toggle_group("a");
        assert_eq!(p.group_state("a"), GroupState::None);
        assert_eq!((p.chosen(), p.selected_size()), (vec![0], 300));
        p.toggle_file(2);
        assert_eq!(p.group_state("a"), GroupState::Some);
        assert_eq!(shown(&p)[0], row("a", -1, "some"));
        // Some selected: the box turns them all on.
        p.toggle_group("a");
        assert_eq!(p.group_state("a"), GroupState::All);
        p.toggle_all(false);
        assert_eq!(p.selected_count(), 0);
        p.toggle_all(true);
        assert_eq!(p.chosen(), [0, 1, 2]);
    }

    #[test]
    fn picker_from_a_job_counts_what_is_left() {
        let progress = |index, path: &str, done, selected| TorrentFileProgress {
            index,
            path: path.into(),
            size: 100,
            done,
            selected,
        };
        let p = Picker::from_progress(&[
            progress(1, "y.bin", 0, false),
            progress(0, "x.bin", 40, true),
        ]);
        assert_eq!(p.chosen(), [0]);
        assert_eq!((p.selected_size(), p.needed()), (100, 60));
    }
}
