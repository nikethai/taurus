//! What a yt-dlp run has shown so far, read from the lines it prints with
//! the templates `run::download_args` gives it.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde_json::Value;

use super::run::MediaPhase;

pub(super) const PROGRESS: &str = "TAURUS-PROGRESS ";
pub(super) const POSTPROCESS: &str = "TAURUS-PP ";
pub(super) const FILE: &str = "TAURUS-FILE ";
pub(super) const ENTRY: &str = "TAURUS-ENTRY ";
/// What every line of ours starts with, on either stream.
pub(super) const OURS: &str = "TAURUS-";

/// What a run has shown so far, from its output lines.
#[derive(Clone, Debug)]
pub(crate) struct Tracker {
    dest_dir: PathBuf,
    pub phase: MediaPhase,
    pub entries_done: u32,
    pub entries_total: u32,
    /// The finished files, in the order yt-dlp moved them into place.
    pub files: Vec<PathBuf>,
    /// The current entry's partial files, newest last: the one yt-dlp
    /// announced (`<filename>.part`, see `ENTRY`) and those it reported
    /// writing to.
    pub parts: Vec<PathBuf>,
    /// Whether any progress line came.
    pub progressed: bool,
    /// Whether the download has begun: a progress line came, or a live
    /// stream's entry was announced (yt-dlp records it through ffmpeg,
    /// which prints no progress at all).
    pub started: bool,
    /// Where each entry goes, as yt-dlp announced it (`%(filename)j` at
    /// `before_dl`), in order.
    pub entry_files: Vec<PathBuf>,
    /// The id of the video downloading now (yt-dlp's `%(id)s`, which the
    /// output names carry as `[<id>]`).
    pub video_id: Option<String>,
    /// The job's unfinished files: every `tmpfilename` reported, and the
    /// `filename` it becomes, until reported finished (`TAURUS-FILE`).
    /// Seeded by the runner with what earlier runs recorded; a finished
    /// entry also drops those that no longer exist (yt-dlp removed them).
    pub partials: Vec<PathBuf>,
    /// Bytes of the entries before the current one.
    before_entry: u64,
    /// Bytes of the current entry's finished streams (video, then audio).
    entry_finished: u64,
    /// The stream downloading now: its file, bytes and size.
    stream: Option<String>,
    stream_done: u64,
    stream_total: Option<u64>,
    speed: u64,
}

/// What one line changed.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Change {
    /// `(downloaded, total, speed)` to report.
    pub progress: Option<(u64, Option<u64>, u64)>,
    /// Phase, entries or files changed.
    pub view: bool,
    /// `partials` changed (to be stored).
    pub partials: bool,
}

impl Tracker {
    pub(crate) fn new(dest_dir: &Path, phase: MediaPhase) -> Tracker {
        Tracker {
            dest_dir: dest_dir.to_path_buf(),
            phase,
            entries_done: 0,
            entries_total: 0,
            files: Vec::new(),
            parts: Vec::new(),
            progressed: false,
            started: false,
            entry_files: Vec::new(),
            video_id: None,
            partials: Vec::new(),
            before_entry: 0,
            entry_finished: 0,
            stream: None,
            stream_done: 0,
            stream_total: None,
            speed: 0,
        }
    }

    /// Reads one line yt-dlp printed (on stdout or stderr); lines that are
    /// not ours change nothing.
    pub(crate) fn apply(&mut self, line: &str) -> Change {
        let line = line.trim_end();
        if let Some(json) = line.strip_prefix(PROGRESS) {
            return serde_json::from_str::<Value>(json).map_or_else(
                |_| Change::default(),
                |progress| self.on_progress(&progress),
            );
        }
        if let Some(rest) = line.strip_prefix(POSTPROCESS) {
            let mut words = rest.split_whitespace();
            let (pp, status) = (words.next().unwrap_or(""), words.next().unwrap_or(""));
            // yt-dlp's `pp_key()`: the class name without `FFmpeg` and `PP`
            // (the prefixed spellings are kept, in case that ever changes).
            let phase = match pp.strip_prefix("FFmpeg").unwrap_or(pp) {
                "Merger" => MediaPhase::Merging,
                "ExtractAudio" | "VideoConvertor" | "VideoRemuxer" => MediaPhase::Converting,
                _ => return Change::default(),
            };
            if status == "finished" {
                return Change::default();
            }
            return self.set_phase(phase);
        }
        if let Some(json) = line.strip_prefix(FILE) {
            let Ok(path) = serde_json::from_str::<String>(json) else {
                return Change::default();
            };
            let path = local_path(&self.dest_dir, &path);
            let recorded = self.partials.len();
            // Finished, it is the user's; what yt-dlp cleaned up is gone.
            self.partials.retain(|p| *p != path && p.exists());
            if !self.files.contains(&path) {
                self.files.push(path);
            }
            let done = u32::try_from(self.files.len()).unwrap_or(u32::MAX);
            self.entries_done = self.entries_done.max(done.min(self.entries_total.max(1)));
            return Change {
                progress: None,
                view: true,
                partials: self.partials.len() != recorded,
            };
        }
        if let Some(rest) = line.strip_prefix(ENTRY) {
            let mut words = rest.splitn(3, ' ');
            let number = |w: Option<&str>| w.and_then(|w| w.parse::<u32>().ok());
            let (Some(index), Some(count)) = (number(words.next()), number(words.next())) else {
                return Change::default();
            };
            // Then the id, the title and the file, each a JSON string.
            let mut strings = serde_json::Deserializer::from_str(words.next().unwrap_or(""))
                .into_iter::<String>();
            let mut text = || strings.next().and_then(Result::ok);
            self.video_id = text().filter(|id| !id.is_empty());
            let _title = text();
            // yt-dlp's placeholder for a field it does not have is `NA`.
            let file = text().filter(|f| !f.is_empty() && f != "NA");
            self.before_entry += self.entry_finished + self.stream_done;
            self.entry_finished = 0;
            self.stream = None;
            self.stream_done = 0;
            self.stream_total = None;
            self.entries_total = count.max(1);
            self.entries_done = self.entries_done.max(index.saturating_sub(1));
            // The entry's own files, known before it is written to.
            self.parts.clear();
            let mut partials = false;
            if let Some(file) = file {
                let file = local_path(&self.dest_dir, &file);
                let mut part = file.clone().into_os_string();
                part.push(".part");
                let part = PathBuf::from(part);
                self.parts.push(part.clone());
                partials = self.record(part);
                if !self.entry_files.contains(&file) {
                    self.entry_files.push(file);
                }
            }
            // A live stream begins here: yt-dlp waited for it before.
            if self.phase == MediaPhase::WaitingForStream {
                self.phase = MediaPhase::Downloading;
                self.started = true;
            }
            return Change {
                progress: None,
                view: true,
                partials,
            };
        }
        Change::default()
    }

    /// The run ended well: every entry is done.
    pub(crate) fn finish(&mut self) {
        self.entries_total = self.entries_total.max(1);
        self.entries_done = self.entries_total;
    }

    /// The run ended with yt-dlp refusing only to embed thumbnails, which
    /// the entries' containers (AVI, WebM, …) cannot hold: it stopped
    /// after the download and the other postprocessing, before moving the
    /// file into place. So every announced entry file that exists is
    /// finished, and what yt-dlp left beside it (see `remove_leftovers`;
    /// `since` is when the job was added: a resumed playlist reuses a
    /// thumbnail an earlier run wrote) is deleted.
    pub(crate) fn keep_without_thumbnails(&mut self, since: SystemTime) {
        for file in self.entry_files.clone() {
            if !file.is_file() {
                continue;
            }
            remove_leftovers_of(&file, since);
            if !self.files.contains(&file) {
                self.files.push(file);
            }
        }
        self.partials
            .retain(|p| !self.files.contains(p) && p.exists());
    }

    /// Deletes what yt-dlp left beside the announced entry files when it
    /// did not finish them (a recording it was killed in): the thumbnail
    /// it wrote to embed (`<stem>.jpg`, `.png` or `.webp`) and the file
    /// its postprocessors rewrite through (`<stem>.temp.<ext>`), each only
    /// if written since `since` (when the job was added).
    pub(crate) fn remove_leftovers(&self, since: SystemTime) {
        for file in &self.entry_files {
            remove_leftovers_of(file, since);
        }
    }

    fn on_progress(&mut self, progress: &Value) -> Change {
        let number = |key: &str| progress.get(key).and_then(Value::as_f64);
        let text = |key: &str| progress.get(key).and_then(Value::as_str);
        let done = number("downloaded_bytes").map_or(0, whole);
        let total = number("total_bytes")
            .or_else(|| number("total_bytes_estimate"))
            .map(whole);
        let file = text("filename").map(str::to_owned);
        let mut partials = false;
        if let Some(part) = text("tmpfilename") {
            let part = local_path(&self.dest_dir, part);
            if self.parts.last() != Some(&part) {
                self.parts.retain(|p| *p != part);
                self.parts.push(part.clone());
            }
            // Only with a `tmpfilename`: this run writes it (a file already
            // downloaded is reported without one, and is not the job's to
            // delete).
            partials |= self.record(part);
            if let Some(file) = &file {
                partials |= self.record(local_path(&self.dest_dir, file));
            }
        }
        // Another stream of the same entry (the audio after the video).
        if file.is_some() && self.stream.is_some() && file != self.stream {
            self.entry_finished += self.stream_done;
            self.stream_done = 0;
        }
        self.stream = file;
        self.speed = number("speed").map_or(0, whole);
        if text("status") == Some("finished") {
            self.entry_finished += total.unwrap_or(done).max(done);
            self.stream = None;
            self.stream_done = 0;
            self.stream_total = None;
            self.speed = 0;
        } else {
            self.stream_done = done;
            self.stream_total = total;
        }
        self.progressed = true;
        self.started = true;
        let view = self.phase != MediaPhase::Downloading;
        self.phase = MediaPhase::Downloading;
        Change {
            progress: Some(self.totals()),
            view,
            partials,
        }
    }

    /// Adds `path` to `partials`; true if it was not there.
    fn record(&mut self, path: PathBuf) -> bool {
        if self.partials.contains(&path) {
            return false;
        }
        self.partials.push(path);
        true
    }

    fn set_phase(&mut self, phase: MediaPhase) -> Change {
        let view = self.phase != phase;
        self.phase = phase;
        Change {
            progress: None,
            view,
            partials: false,
        }
    }

    /// `(downloaded, total, speed)` of the whole job: for a playlist the
    /// entries done plus the current one's share, scaled to bytes when its
    /// size is known (else the bytes so far, with no total).
    fn totals(&self) -> (u64, Option<u64>, u64) {
        let entry_done = self.entry_finished + self.stream_done;
        let entry_total = self
            .stream_total
            .map(|t| self.entry_finished + t.max(self.stream_done));
        if self.entries_total <= 1 {
            return (entry_done, entry_total, self.speed);
        }
        match entry_total {
            Some(t) if t > 0 => {
                let share = entry_done as f64 / t as f64;
                let total = t.saturating_mul(u64::from(self.entries_total));
                let fraction =
                    (f64::from(self.entries_done) + share) / f64::from(self.entries_total);
                (whole(fraction * total as f64), Some(total), self.speed)
            }
            _ => (self.before_entry + entry_done, None, self.speed),
        }
    }
}

/// Deletes yt-dlp's leftovers of `file` (see `Tracker::remove_leftovers`)
/// written since `since` (an older one is not this job's).
fn remove_leftovers_of(file: &Path, since: SystemTime) {
    let since = since.checked_sub(Duration::from_secs(2)).unwrap_or(since);
    let mut leftovers: Vec<PathBuf> = ["jpg", "png", "webp"]
        .iter()
        .map(|ext| file.with_extension(ext))
        .collect();
    if let Some(ext) = file.extension() {
        let mut temp = std::ffi::OsString::from("temp.");
        temp.push(ext);
        leftovers.push(file.with_extension(temp));
    }
    for leftover in leftovers {
        if leftover != file && written_since(&leftover, since) {
            let _ = std::fs::remove_file(&leftover);
        }
    }
}

/// Whether `path` is a file last written at or after `since`.
pub(crate) fn written_since(path: &Path, since: SystemTime) -> bool {
    std::fs::metadata(path)
        .ok()
        .filter(std::fs::Metadata::is_file)
        .and_then(|m| m.modified().ok())
        .is_some_and(|modified| modified >= since)
}

/// A path yt-dlp printed (absolute, or relative to its working folder,
/// `dest_dir`) in one spelling: no `\\?\` prefix where a plain path does,
/// and the platform's separator throughout (yt-dlp may mix `/` into a
/// Windows path from the output template).
pub(crate) fn local_path(dest_dir: &Path, printed: &str) -> PathBuf {
    dunce::simplified(&dest_dir.join(printed))
        .components()
        .collect()
}

fn whole(n: f64) -> u64 {
    if n.is_finite() && n > 0.0 {
        n as u64
    } else {
        0
    }
}
