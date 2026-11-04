//! Advanced mode's Add several links screen: the text is parsed
//! (`batch.rs`) after each edit; "Add N downloads" adds the lines in order
//! (`grab_flow::add_all`), each in the folder its kind is sorted into.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use slint::{ComponentHandle, ModelRc, VecModel};
use taurus_core::{AddRequest, JobKind};

use crate::app::App;
use crate::batch::{self, BatchLine, LineKind};
use crate::pump::set_model;
use crate::{AppWindow, BatchLineVm, BatchVm, LinksUi, Screen, rows};

/// Several links: the text as last edited and what it says.
#[derive(Default)]
struct Batch {
    text: String,
    mirrors: bool,
    lines: Vec<BatchLine>,
}

/// The screen's state; the list model is the window's.
#[derive(Default)]
pub(crate) struct BatchState {
    lines: Rc<VecModel<BatchLineVm>>,
    batch: RefCell<Batch>,
}

impl BatchState {
    pub fn install(&self, ui: &AppWindow) {
        ui.global::<LinksUi>()
            .set_batch_lines(ModelRc::from(self.lines.clone()));
    }
}

/// The kind a download is sorted by (see `add_flow::sort_folder`).
fn sort_kind(line: &BatchLine) -> &'static str {
    match line.kind {
        LineKind::Torrent => "magnet",
        LineKind::Media => "media",
        _ => {
            let name = url::Url::parse(&line.text)
                .ok()
                .and_then(|u| u.path_segments()?.next_back().map(str::to_string))
                .unwrap_or_default();
            rows::kind_for(&name, JobKind::Http)
        }
    }
}

impl App {
    /// Opens Add several links, empty.
    pub(crate) fn open_batch(&self) {
        let Some(ui) = self.window.upgrade() else {
            return;
        };
        *self.batch.batch.borrow_mut() = Batch::default();
        self.render_batch(&ui);
        ui.set_screen(Screen::Batch);
    }

    pub(crate) fn batch_edited(&self, text: &str) {
        self.batch.batch.borrow_mut().text = text.to_string();
        self.reparse_batch();
    }

    pub(crate) fn batch_mirrors(&self, on: bool) {
        self.batch.batch.borrow_mut().mirrors = on;
        self.reparse_batch();
    }

    fn reparse_batch(&self) {
        let Some(ui) = self.window.upgrade() else {
            return;
        };
        {
            let mut b = self.batch.batch.borrow_mut();
            b.lines = batch::parse_batch(&b.text, b.mirrors);
        }
        self.render_batch(&ui);
    }

    fn render_batch(&self, ui: &AppWindow) {
        let b = self.batch.batch.borrow();
        let number = |n: usize| n.min(i32::MAX as usize) as i32;
        let lines = b.lines.iter().map(|l| BatchLineVm {
            n: number(l.n),
            text: l.text.as_str().into(),
            kind: l.kind.key().into(),
            mirror_of: l.mirror_of.map_or(0, number),
        });
        set_model(&self.batch.lines, lines, |l| l.n);
        ui.global::<LinksUi>().set_batch(BatchVm {
            add_count: number(b.lines.iter().filter(|l| batch::is_added(l)).count()),
            mirrors: b.mirrors,
        });
    }

    /// "Add N downloads": every added line, in order, each in the folder
    /// its kind is sorted into.
    pub(crate) fn batch_add(&self) {
        let lines = std::mem::take(&mut self.batch.batch.borrow_mut().lines);
        let base = self.default_destination("").0;
        let requests: Vec<(AddRequest, Option<PathBuf>)> = batch::to_requests(&lines, &base)
            .into_iter()
            .zip(lines.iter().filter(|l| batch::is_added(l)))
            .map(|(mut request, line)| {
                let (dest_dir, sorted) = self.default_destination(sort_kind(line));
                request.dest_dir = dest_dir;
                (request, sorted.then(|| base.clone()))
            })
            .collect();
        if requests.is_empty() {
            self.batch.batch.borrow_mut().lines = lines;
            return;
        }
        *self.batch.batch.borrow_mut() = Batch::default();
        if let Some(ui) = self.window.upgrade() {
            self.render_batch(&ui);
        }
        self.add_all(requests);
    }
}
