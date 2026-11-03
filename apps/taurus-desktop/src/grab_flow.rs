//! Advanced mode's Grab links screen (and `add_all`, which Add several
//! links in `batch_flow.rs` uses too).
//!
//! Grab links: `Engine::grab` reads the page (on the runtime); its links
//! go into the list at once, without sizes, and `probe_sizes_stream` then
//! fills the sizes in as they arrive. Each 100 ms tick takes every size
//! that arrived and changes only those rows (`set_row_data`), so a page
//! of 500 links never rebuilds its list and never holds up the window.
//! Every grab has a run number; a new page, the Subfolders switch or
//! leaving the screen starts a new run and drops the old probes (their
//! receiver), so nothing of an older run reaches the list.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use slint::{Model, ModelRc, VecModel};
use taurus_core::grab::folder_in;
use taurus_core::{
    AddRequest, CoreError, Engine, ErrorKind, GrabOptions, GrabResult, GrabbedLink, JobKind,
};
use tokio::sync::mpsc::{self, error::TryRecvError};

use crate::app::{App, UiMsg};
use slint::ComponentHandle;

use crate::{AppWindow, GrabRowVm, GrabVm, LinksUi, Screen, format, rows};

/// Extensions listed under "Docs".
const DOCS: &[&str] = &[
    "pdf", "doc", "docx", "odt", "rtf", "txt", "md", "epub", "xls", "xlsx", "ods", "csv", "ppt",
    "pptx", "odp",
];

/// The chip a file is listed under besides "all": "video", "docs",
/// "archives", "images", or "" for none.
pub fn chip_of(name: &str) -> &'static str {
    match rows::kind_for(name, JobKind::Http) {
        "media" => "video",
        "image" => "images",
        "archive" | "disc" => "archives",
        _ => {
            let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase());
            if ext.is_some_and(|e| DOCS.contains(&e.as_str())) {
                "docs"
            } else {
                ""
            }
        }
    }
}

/// Whether `ext` is one of the extensions typed in `filter` (separated by
/// spaces, commas or semicolons; dots and case ignored). An empty filter
/// takes every link.
pub fn ext_matches(filter: &str, ext: Option<&str>) -> bool {
    let mut wanted = filter
        .split([' ', ',', ';'])
        .map(|w| w.trim().trim_start_matches('.'))
        .filter(|w| !w.is_empty())
        .peekable();
    if wanted.peek().is_none() {
        return true;
    }
    let Some(ext) = ext else {
        return false;
    };
    wanted.any(|w| w.eq_ignore_ascii_case(ext))
}

/// The Grab links screen.
struct Grab {
    run: u64,
    page_url: String,
    subfolders: bool,
    loading: bool,
    error: Option<ErrorKind>,
    truncated: bool,
    links: Vec<GrabbedLink>,
    chips: Vec<&'static str>,
    /// Each link's size: `None` until probed, `Some(None)` when the
    /// server told none.
    sizes: Vec<Option<Option<u64>>>,
    selected: Vec<bool>,
    chip: String,
    ext: String,
    /// The link of each list row, in order.
    shown: Vec<usize>,
    /// The list row of each link, when shown.
    position: Vec<Option<usize>>,
    probes: Option<mpsc::Receiver<(usize, Option<u64>)>>,
}

impl Default for Grab {
    fn default() -> Self {
        Grab {
            run: 0,
            page_url: String::new(),
            subfolders: false,
            loading: false,
            error: None,
            truncated: false,
            links: Vec::new(),
            chips: Vec::new(),
            sizes: Vec::new(),
            selected: Vec::new(),
            chip: "all".into(),
            ext: String::new(),
            shown: Vec::new(),
            position: Vec::new(),
            probes: None,
        }
    }
}

impl Grab {
    /// Forgets the links (and stops their probes) for a new run.
    fn reset_links(&mut self) {
        self.run += 1;
        self.probes = None;
        self.error = None;
        self.truncated = false;
        self.links.clear();
        self.chips.clear();
        self.sizes.clear();
        self.selected.clear();
        self.shown.clear();
        self.position.clear();
    }

    fn row(&self, i: usize) -> GrabRowVm {
        let link = &self.links[i];
        let size = self.sizes[i];
        GrabRowVm {
            index: i.min(i32::MAX as usize) as i32,
            name: link.name.as_str().into(),
            folder: link.folder.as_str().into(),
            kind: rows::kind_for(&link.name, JobKind::Http).into(),
            size_text: size.flatten().map(format::bytes).unwrap_or_default().into(),
            probed: size.is_some(),
            selected: self.selected[i],
        }
    }

    fn is_shown(&self, i: usize) -> bool {
        (self.chip == "all" || self.chips[i] == self.chip)
            && ext_matches(&self.ext, self.links[i].extension.as_deref())
    }

    /// Lists the links the chip and extension filter show.
    fn refilter(&mut self, model: &VecModel<GrabRowVm>) {
        self.shown = (0..self.links.len())
            .filter(|&i| self.is_shown(i))
            .collect();
        self.position = vec![None; self.links.len()];
        for (row, &i) in self.shown.iter().enumerate() {
            self.position[i] = Some(row);
        }
        model.set_vec(self.shown.iter().map(|&i| self.row(i)).collect::<Vec<_>>());
    }

    /// Shows link `i` as it is now, if it is listed.
    fn update_row(&self, model: &VecModel<GrabRowVm>, i: usize) {
        if let Some(row) = self.position.get(i).copied().flatten() {
            model.set_row_data(row, self.row(i));
        }
    }

    /// The screen's view model; `base` is the download folder the
    /// links' subfolders are checked against.
    fn vm(&self, base: &Path) -> GrabVm {
        let count = |chip: &str| {
            let n = self.chips.iter().filter(|c| **c == chip).count();
            n.min(i32::MAX as usize) as i32
        };
        let picked: Vec<usize> = (0..self.links.len())
            .filter(|&i| self.selected[i])
            .collect();
        let size: u64 = picked.iter().filter_map(|&i| self.sizes[i].flatten()).sum();
        let unknown = picked
            .iter()
            .filter(|&&i| self.sizes[i].flatten().is_none())
            .count();
        let refused = picked
            .iter()
            .filter(|&&i| folder_in(base, &self.links[i].folder).is_err())
            .count();
        GrabVm {
            page_url: self.page_url.as_str().into(),
            loading: self.loading,
            error_kind: self.error.map(ErrorKind::as_str).unwrap_or_default().into(),
            subfolders: self.subfolders,
            chip: self.chip.as_str().into(),
            ext_filter: self.ext.as_str().into(),
            count_all: self.links.len().min(i32::MAX as usize) as i32,
            count_video: count("video"),
            count_docs: count("docs"),
            count_archives: count("archives"),
            count_images: count("images"),
            shown_count: self.shown.len().min(i32::MAX as usize) as i32,
            selected_count: picked.len().min(i32::MAX as usize) as i32,
            selected_size: format::bytes(size).into(),
            unknown_count: unknown.min(i32::MAX as usize) as i32,
            unsafe_count: refused.min(i32::MAX as usize) as i32,
            all_shown_selected: self.shown.iter().all(|&i| self.selected[i]),
            truncated: self.truncated,
        }
    }
}

/// The Grab links screen's state; the list model is the window's.
#[derive(Default)]
pub(crate) struct GrabState {
    rows: Rc<VecModel<GrabRowVm>>,
    grab: RefCell<Grab>,
}

impl GrabState {
    pub fn install(&self, ui: &AppWindow) {
        let links = ui.global::<LinksUi>();
        links.set_grab_rows(ModelRc::from(self.rows.clone()));
    }
}

impl App {
    fn render_grab(&self, ui: &AppWindow) {
        let base = self.default_destination("").0;
        ui.global::<LinksUi>()
            .set_grab(self.grab.grab.borrow().vm(&base));
    }

    /// Opens Grab links, reading `url` when it is not empty. An Add
    /// screen it was opened from ("Find files →") is left.
    pub(crate) fn open_grab(&self, url: &str) {
        let Some(ui) = self.window.upgrade() else {
            return;
        };
        self.pending.borrow_mut().take();
        {
            let mut g = self.grab.grab.borrow_mut();
            g.reset_links();
            g.loading = false;
            g.subfolders = false;
            g.chip = "all".into();
            g.ext.clear();
            g.page_url = url.trim().to_string();
            self.grab.rows.set_vec(Vec::new());
        }
        ui.global::<LinksUi>().set_grab_url_text(url.trim().into());
        ui.set_screen(Screen::Grab);
        if url.trim().is_empty() {
            self.render_grab(&ui);
        } else {
            self.start_grab(&ui);
        }
    }

    /// The link field: reads that page.
    pub(crate) fn grab_page(&self, url: &str) {
        let Some(ui) = self.window.upgrade() else {
            return;
        };
        let url = url.trim();
        if url.is_empty() {
            return;
        }
        self.grab.grab.borrow_mut().page_url = url.to_string();
        self.start_grab(&ui);
    }

    /// Reads the page again from scratch (a new run).
    fn start_grab(&self, ui: &AppWindow) {
        let (run, url, subfolders) = {
            let mut g = self.grab.grab.borrow_mut();
            g.reset_links();
            g.loading = true;
            self.grab.rows.set_vec(Vec::new());
            (g.run, g.page_url.clone(), g.subfolders)
        };
        self.render_grab(ui);
        let (engine, tx) = (self.engine.clone(), self.tx.clone());
        self.rt.spawn(async move {
            let result = engine
                .grab(&url, GrabOptions { subfolders })
                .await
                .map_err(|e| e.kind);
            tx.send(UiMsg::Grabbed {
                run,
                result: Box::new(result),
            });
        });
    }

    /// `Engine::grab` answered run `run`: every link is listed at once,
    /// and their sizes are asked for. An older run's answer is dropped.
    pub(crate) fn grabbed(&self, run: u64, result: Result<GrabResult, ErrorKind>) {
        let Some(ui) = self.window.upgrade() else {
            return;
        };
        let mut g = self.grab.grab.borrow_mut();
        if g.run != run || !g.loading {
            return;
        }
        g.loading = false;
        match result {
            Ok(found) => {
                let n = found.links.len();
                g.truncated = found.truncated;
                g.chips = found.links.iter().map(|l| chip_of(&l.name)).collect();
                g.sizes = vec![None; n];
                g.selected = vec![false; n];
                g.links = found.links;
                g.refilter(&self.grab.rows);
                if n > 0 {
                    let urls = g.links.iter().map(|l| l.url.clone()).collect();
                    // The probes are spawned on the runtime (ledger T1#11).
                    let _rt = self.rt.enter();
                    g.probes = Some(self.engine.probe_sizes_stream(urls));
                }
            }
            Err(kind) => g.error = Some(kind),
        }
        drop(g);
        self.render_grab(&ui);
    }

    /// Every size that arrived since the last tick, into its row. Runs
    /// before the tick's messages, so a run's sizes always come at least
    /// one tick after its rows.
    pub(crate) fn grab_sizes(&self) {
        let mut g = self.grab.grab.borrow_mut();
        let Some(probes) = g.probes.as_mut() else {
            return;
        };
        let mut arrived = Vec::new();
        let finished = loop {
            match probes.try_recv() {
                Ok(answer) => arrived.push(answer),
                Err(TryRecvError::Empty) => break false,
                Err(TryRecvError::Disconnected) => break true,
            }
        };
        if finished {
            g.probes = None;
        }
        let mut footer = false;
        for (i, size) in arrived {
            if i >= g.sizes.len() {
                continue;
            }
            g.sizes[i] = Some(size);
            footer |= g.selected[i];
            g.update_row(&self.grab.rows, i);
        }
        drop(g);
        if footer && let Some(ui) = self.window.upgrade() {
            self.render_grab(&ui);
        }
    }

    /// "subfolders" reads the page again; "chip" and "ext" filter the list.
    pub(crate) fn grab_option(&self, key: &str, value: &str) {
        let Some(ui) = self.window.upgrade() else {
            return;
        };
        let mut g = self.grab.grab.borrow_mut();
        match key {
            "subfolders" => {
                g.subfolders = value == "true";
                // The link as it reads in the field now (typed or not).
                let typed = ui.global::<LinksUi>().get_grab_url_text();
                if !typed.trim().is_empty() {
                    g.page_url = typed.trim().to_string();
                }
                if !g.page_url.is_empty() {
                    drop(g);
                    return self.start_grab(&ui);
                }
            }
            "chip" if ["all", "video", "docs", "archives", "images"].contains(&value) => {
                g.chip = value.to_string();
                g.refilter(&self.grab.rows);
            }
            "ext" => {
                g.ext = value.to_string();
                g.refilter(&self.grab.rows);
            }
            _ => return,
        }
        drop(g);
        self.render_grab(&ui);
    }

    pub(crate) fn grab_toggle(&self, index: i32) {
        let Some(ui) = self.window.upgrade() else {
            return;
        };
        let mut g = self.grab.grab.borrow_mut();
        let Some(i) = usize::try_from(index)
            .ok()
            .filter(|&i| i < g.selected.len())
        else {
            return;
        };
        g.selected[i] = !g.selected[i];
        g.update_row(&self.grab.rows, i);
        drop(g);
        self.render_grab(&ui);
    }

    /// Ticks every listed link, or unticks them all when they all are.
    pub(crate) fn grab_select_shown(&self) {
        let Some(ui) = self.window.upgrade() else {
            return;
        };
        let mut g = self.grab.grab.borrow_mut();
        let on = !g.shown.iter().all(|&i| g.selected[i]);
        for row in 0..g.shown.len() {
            let i = g.shown[row];
            if g.selected[i] != on {
                g.selected[i] = on;
                g.update_row(&self.grab.rows, i);
            }
        }
        drop(g);
        self.render_grab(&ui);
    }

    /// Download N: one web download per ticked link, in page order, each
    /// in its subfolder (as listed) of the folder its kind is sorted into.
    /// A link whose subfolder name is not safe (`grab::folder_in`) is left
    /// out alone; the footer said so before.
    pub(crate) fn grab_download(&self) {
        let picked: Vec<GrabbedLink> = {
            let g = self.grab.grab.borrow();
            (0..g.links.len())
                .filter(|&i| g.selected[i])
                .map(|i| g.links[i].clone())
                .collect()
        };
        if picked.is_empty() {
            return;
        }
        let base = self.default_destination("").0;
        let requests = picked
            .into_iter()
            .filter_map(|link| {
                let kind = rows::kind_for(&link.name, JobKind::Http);
                let sort_dest = self.default_destination(kind).0;
                let dest_dir = match folder_in(&sort_dest, &link.folder) {
                    Ok(dir) => dir,
                    Err(e) => {
                        eprintln!("hyades: {}: {}", link.url, e.detail);
                        return None;
                    }
                };
                let root = (dest_dir != base).then(|| base.clone());
                let request = AddRequest {
                    url: link.url,
                    dest_dir,
                    ..Default::default()
                };
                Some((request, root))
            })
            .collect();
        self.grab_closed();
        self.add_all(requests);
    }

    /// The screen is left: its probes stop and its links are forgotten.
    pub(crate) fn grab_closed(&self) {
        let mut g = self.grab.grab.borrow_mut();
        g.reset_links();
        g.loading = false;
        self.grab.rows.set_vec(Vec::new());
        drop(g);
        self.next_queued();
    }

    /// Shows the list and adds `requests` one after the other; the first
    /// refusal, if any, is shown as an add the engine refused. A request
    /// with a `root` folder gets its `dest_dir` (below `root`) made first,
    /// when `root` exists (a missing drive is reported, not created).
    pub(crate) fn add_all(&self, requests: Vec<(AddRequest, Option<PathBuf>)>) {
        if let Some(ui) = self.window.upgrade() {
            ui.set_screen(Screen::Downloads);
        }
        let (engine, tx) = (self.engine.clone(), self.tx.clone());
        self.rt.spawn(async move {
            let mut refused = None;
            for (request, root) in requests {
                if let Err(CoreError { kind, detail }) = add_below(&engine, request, root).await {
                    eprintln!("hyades: add failed: {detail}");
                    refused.get_or_insert(kind);
                }
            }
            if let Some(kind) = refused {
                tx.send(UiMsg::AddFailed(kind));
            }
        });
    }
}

/// Adds `request`, its folder made first below `root` when `root` exists.
async fn add_below(
    engine: &Engine,
    request: AddRequest,
    root: Option<PathBuf>,
) -> Result<taurus_core::JobId, CoreError> {
    if let Some(root) = root
        && root.is_dir()
        && request.dest_dir.starts_with(&root)
    {
        let _ = tokio::fs::create_dir_all(&request.dest_dir).await;
    }
    engine.add(request).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chips_by_extension() {
        for (name, chip) in [
            ("talk.MP4", "video"),
            ("clip.webm", "video"),
            ("paper.pdf", "docs"),
            ("notes.TXT", "docs"),
            ("src.tar.gz", "archives"),
            ("disk.iso", "archives"),
            ("photo.jpeg", "images"),
            ("song.mp3", ""),
            ("setup.exe", ""),
            ("README", ""),
        ] {
            assert_eq!(chip_of(name), chip, "{name}");
        }
    }

    #[test]
    fn extension_filter() {
        assert!(ext_matches("", None));
        assert!(ext_matches("  ", Some("pdf")));
        assert!(ext_matches("pdf", Some("pdf")));
        assert!(ext_matches(".PDF, zip", Some("zip")));
        assert!(ext_matches("mp4;mkv", Some("mkv")));
        assert!(!ext_matches("pdf", Some("pdfx")));
        assert!(!ext_matches("pdf", None));
    }
}
