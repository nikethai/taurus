//! A job's view and details turned into the Advanced detail (pure): the
//! segment map, torrent file rows, media steps and "label: value" pairs.
//! Numbers are formatted here; the detail pane words every label.

use slint::{ModelRc, VecModel};
use taurus_core::media::MediaPhase;
use taurus_core::{
    ErrorKind, JobDetail, JobKind, JobState, JobView, PasswordStore, ProgressUnit, SegmentView,
    TorrentOverrides,
};

use crate::{DetailVm, FileRowVm, StatVm, StepVm, format, rows};

/// Cells of the segment map, as in the canvas.
pub const CELLS: usize = 96;

/// How much of each of `cells` equal parts of `[0, total)` the segments
/// have written, 0..1 per cell. A segment's written bytes are
/// `[start, start + done)`; whatever lies past `total` is left out (a
/// download of unknown size reports one segment `[0, u64::MAX)`). No cells
/// when `total` or `cells` is 0.
pub fn segment_cells(segments: &[SegmentView], total: u64, cells: usize) -> Vec<f32> {
    if total == 0 || cells == 0 {
        return Vec::new();
    }
    let total = u128::from(total);
    let written: Vec<(u128, u128)> = segments
        .iter()
        .map(|s| {
            let start = u128::from(s.start).min(total);
            let end = u128::from(s.end).min(total);
            let written = (start + u128::from(s.done)).min(end);
            (start, written)
        })
        .filter(|(start, end)| end > start)
        .collect();
    let n = cells as u128;
    (0..n)
        .map(|i| {
            let (lo, hi) = (total * i / n, total * (i + 1) / n);
            if hi == lo {
                // Fewer bytes than cells: the cell shows its one byte.
                let covered = written.iter().any(|&(s, e)| s <= lo && lo < e);
                return if covered { 1.0 } else { 0.0 };
            }
            let done: u128 = written
                .iter()
                .map(|&(s, e)| e.min(hi).saturating_sub(s.max(lo)))
                .sum();
            (done as f64 / (hi - lo) as f64).clamp(0.0, 1.0) as f32
        })
        .collect()
}

/// The detail of `view` with its `detail`; `now` is the current time in
/// Unix seconds (for the row's finish time).
pub fn detail_vm(view: &JobView, detail: &JobDetail, now: i64) -> DetailVm {
    let row = rows::row_of(view, now);
    let size_line = if row.total_text.is_empty() {
        row.done_text.to_string()
    } else {
        format!("{} / {}", row.done_text, row.total_text)
    };
    let error = view.error.as_ref().map(|(k, _)| *k);
    let failed_with = |kinds: &[ErrorKind]| {
        view.state == JobState::Failed && error.is_some_and(|k| kinds.contains(&k))
    };
    let kind = rows::row_kind(view);
    let overrides = view
        .torrent
        .as_ref()
        .map(|t| t.overrides.clone())
        .or_else(|| detail.overrides.clone())
        .unwrap_or_default();
    let cells = cells_of(view, detail);
    DetailVm {
        visible: true,
        name: view.file_name.as_str().into(),
        kind: kind.into(),
        source: rows::source_of(view.kind).into(),
        big: percent(view)
            .map(|p| format!("{p}%"))
            .unwrap_or_default()
            .into(),
        size_line: size_line.into(),
        has_cells: !cells.is_empty(),
        cells: model(cells),
        files: model(files_of(view, detail)),
        steps: model(steps_of(view)),
        stats: model(stats_of(view, detail)),
        folder: view.dest_dir.display().to_string().into(),
        url: link_of(view).into(),
        can_convert: view.state == JobState::Completed
            && match view.kind {
                // The engine converts the files it knows of.
                JobKind::Media => !detail.media_files.is_empty(),
                JobKind::Http => matches!(kind, "media" | "music"),
                _ => false,
            },
        can_retry_cookies: view.kind == JobKind::Media
            && failed_with(&[ErrorKind::NeedsSignIn, ErrorKind::BlockedByBotCheck]),
        needs_password: view.kind == JobKind::Http && failed_with(&[ErrorKind::NeedsSignIn]),
        can_choose_files: view.kind == JobKind::Torrent && !detail.files.is_empty(),
        ratio_override: ratio_text(overrides.stop_ratio).into(),
        seed_time_override: secs_text(overrides.stop_after_secs).into(),
        overrides_epoch: 0,
        row,
    }
}

/// The link a job was added with, as the detail shows and copies it:
/// without any user name or password in it. A conversion has none ("").
pub fn link_of(view: &JobView) -> String {
    if view.kind == JobKind::Convert {
        return String::new();
    }
    match url::Url::parse(&view.source_url) {
        Ok(mut url) if !url.username().is_empty() || url.password().is_some() => {
            // Only fails for links that cannot have a user (no host).
            let _ = url.set_username("");
            let _ = url.set_password(None);
            url.to_string()
        }
        Ok(_) => view.source_url.clone(),
        Err(_) => strip_userinfo(&view.source_url),
    }
}

/// A link the URL parser refuses, without a `user[:password]@` after its
/// `scheme://` (plain string handling: the authority ends at the first
/// `/`, `?` or `#`; its last `@` ends the user part).
fn strip_userinfo(link: &str) -> String {
    let Some(i) = link.find("://") else {
        return link.to_owned();
    };
    let (head, rest) = link.split_at(i + 3);
    let authority = &rest[..rest.find(['/', '?', '#']).unwrap_or(rest.len())];
    match authority.rfind('@') {
        Some(at) => format!("{head}{}", &rest[at + 1..]),
        None => link.to_owned(),
    }
}

/// `text` with every link in it (each word holding a `scheme://`)
/// stripped of its `user[:password]@`.
pub(crate) fn scrub_links(text: &str) -> String {
    text.split_inclusive(char::is_whitespace)
        .map(strip_userinfo)
        .collect()
}

/// Prints `line` to stderr as Hyades' own, without the user name and
/// password of any link in it (an error's detail can quote its link).
pub(crate) fn warn(line: std::fmt::Arguments) {
    eprintln!("hyades: {}", scrub_links(&line.to_string()));
}

fn model<T: Clone + 'static>(items: Vec<T>) -> ModelRc<T> {
    ModelRc::new(VecModel::from(items))
}

/// Percent done; `None` when the size is unknown. The unit is the view's:
/// a conversion's last progress carries bytes (more than its length in
/// milliseconds) while the view still counts time: it is finishing.
fn percent(view: &JobView) -> Option<u64> {
    if view.state == JobState::Completed {
        return Some(100);
    }
    let total = view.total.filter(|&t| t > 0)?;
    let done = view.downloaded.min(total);
    Some((u128::from(done) * 100 / u128::from(total)) as u64)
}

/// A web download's segment map: full once it completed, else from its
/// segments over its known size (none while the size is unknown).
fn cells_of(view: &JobView, detail: &JobDetail) -> Vec<f32> {
    if view.kind != JobKind::Http || view.progress_unit != ProgressUnit::Bytes {
        return Vec::new();
    }
    match view.total.filter(|&t| t > 0) {
        Some(_) if view.state == JobState::Completed => vec![1.0; CELLS],
        Some(total) => segment_cells(&detail.segments, total, CELLS),
        None => Vec::new(),
    }
}

/// A torrent's files, or a media job's finished files.
fn files_of(view: &JobView, detail: &JobDetail) -> Vec<FileRowVm> {
    if view.kind == JobKind::Torrent {
        return detail
            .files
            .iter()
            .map(|f| FileRowVm {
                index: f.index.min(i32::MAX as usize) as i32,
                name: f.path.as_str().into(),
                size_text: format::bytes(f.size).into(),
                progress: if f.size == 0 {
                    1.0
                } else {
                    (f.done.min(f.size) as f64 / f.size as f64) as f32
                },
                selected: f.selected,
            })
            .collect();
    }
    detail
        .media_files
        .iter()
        .map(|p| FileRowVm {
            index: -1,
            name: p
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| p.display().to_string())
                .into(),
            size_text: Default::default(),
            progress: 1.0,
            selected: true,
        })
        .collect()
}

/// A media job's steps: getting the tools, then downloading and finishing
/// (merging or converting), or for a live stream waiting for it and
/// recording.
fn steps_of(view: &JobView) -> Vec<StepVm> {
    let Some(m) = &view.media else {
        return Vec::new();
    };
    let keys: [&str; 3] = if m.live {
        ["tools", "waiting", "record"]
    } else {
        ["tools", "download", "finish"]
    };
    let at = match m.phase {
        MediaPhase::GettingTools => 0,
        _ if view.state == JobState::Recording => 2,
        MediaPhase::WaitingForStream | MediaPhase::Downloading => 1,
        MediaPhase::Merging | MediaPhase::Converting => 2,
    };
    keys.iter()
        .enumerate()
        .map(|(i, key)| {
            let state = if view.state == JobState::Completed || i < at {
                "done"
            } else if i > at {
                "todo"
            } else if view.state == JobState::Failed {
                "failed"
            } else {
                "current"
            };
            StepVm {
                key: (*key).into(),
                state: state.into(),
            }
        })
        .collect()
}

fn stat(key: &str, value: impl Into<slint::SharedString>) -> StatVm {
    StatVm {
        key: key.into(),
        value: value.into(),
    }
}

fn stats_of(view: &JobView, detail: &JobDetail) -> Vec<StatVm> {
    let mut out = Vec::new();
    let running = matches!(view.state, JobState::Downloading | JobState::Recording)
        && view.progress_unit == ProgressUnit::Bytes;
    if running && view.speed_bps > 0 {
        out.push(stat("speed", format::speed(view.speed_bps)));
    }
    match view.kind {
        JobKind::Http => {
            if detail.connections > 0 {
                out.push(stat("connections", detail.connections.to_string()));
            }
            if !detail.segments.is_empty() && view.total.is_some() {
                out.push(stat("segments", detail.segments.len().to_string()));
            }
            if !detail.mirrors.is_empty() {
                out.push(stat("mirrors", detail.mirrors.len().to_string()));
            }
            if let Some(sum) = &detail.checksum {
                out.push(stat("checksum", sum.as_str()));
            }
            if let Some(referer) = &detail.referer {
                out.push(stat("referer", referer.as_str()));
            }
            match detail.password_store {
                Some(PasswordStore::Keychain) => out.push(stat("password", "keychain")),
                Some(PasswordStore::Memory) => out.push(stat("password", "memory")),
                None => {}
            }
        }
        JobKind::Torrent => {
            if let Some(t) = &view.torrent {
                out.push(stat("peers", t.peers.to_string()));
                out.push(stat("uploaded", format::bytes(t.uploaded)));
                if t.upload_bps > 0 {
                    out.push(stat("upload-speed", format::speed(t.upload_bps)));
                }
                out.push(stat("ratio", format::ratio(t.ratio)));
                out.push(stat(
                    "files",
                    format!("{}/{}", t.files_selected, t.files_total),
                ));
            }
        }
        JobKind::Media => {
            if let Some(m) = &view.media {
                if m.entries_total > 1 {
                    out.push(stat(
                        "entries",
                        format!("{}/{}", m.entries_done, m.entries_total),
                    ));
                }
                if let Some(secs) = m.live_elapsed_secs {
                    out.push(stat("recorded", format::duration(secs)));
                }
            }
        }
        JobKind::Convert => {}
    }
    out
}

/// An override as the selects show it: "default" (the global setting),
/// "never", or the number.
pub fn ratio_text(o: Option<Option<f64>>) -> String {
    match o {
        None => "default".into(),
        Some(None) => "never".into(),
        Some(Some(r)) if r.fract() == 0.0 => format!("{r:.0}"),
        Some(Some(r)) => format!("{r}"),
    }
}

/// See `ratio_text`; seconds.
pub fn secs_text(o: Option<Option<u64>>) -> String {
    match o {
        None => "default".into(),
        Some(None) => "never".into(),
        Some(Some(s)) => s.to_string(),
    }
}

/// `current` with `field` ("ratio" or "seed-time") set from `value`
/// ("default", "never" or a number, as `ratio_text` / `secs_text` write
/// them); the other field is kept. `None` for anything else.
pub fn with_override(
    current: &TorrentOverrides,
    field: &str,
    value: &str,
) -> Option<TorrentOverrides> {
    fn parse<T: std::str::FromStr>(value: &str) -> Option<Option<Option<T>>> {
        match value {
            "default" => Some(None),
            "never" => Some(Some(None)),
            v => v.parse().ok().map(|n| Some(Some(n))),
        }
    }
    let mut out = current.clone();
    match field {
        "ratio" => {
            let ratio = parse::<f64>(value)?;
            if ratio.flatten().is_some_and(|r| !r.is_finite() || r < 0.0) {
                return None;
            }
            out.stop_ratio = ratio;
        }
        "seed-time" => out.stop_after_secs = parse::<u64>(value)?,
        _ => return None,
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::view;
    use slint::Model;
    use std::path::PathBuf;
    use taurus_core::media::MediaPhase;
    use taurus_core::{JobId, MediaView, TorrentFileProgress, TorrentView};

    fn seg(start: u64, end: u64, done: u64) -> SegmentView {
        SegmentView { start, end, done }
    }

    fn detail(id: i64) -> JobDetail {
        JobDetail {
            id: JobId(id),
            segments: Vec::new(),
            connections: 0,
            files: Vec::new(),
            media_files: Vec::new(),
            mirrors: Vec::new(),
            referer: None,
            checksum: None,
            has_password: false,
            password_store: None,
            overrides: None,
        }
    }

    fn items<T: Clone + 'static>(m: &ModelRc<T>) -> Vec<T> {
        m.iter().collect()
    }

    #[test]
    fn segment_cells_fill() {
        // Two segments of 960 bytes, each half written, over 96 cells of
        // 20 bytes: 24 full cells, 24 empty, 24 full, 24 empty.
        let segments = [seg(0, 960, 480), seg(960, 1920, 480)];
        let cells = segment_cells(&segments, 1920, 96);
        let mut want = vec![1.0f32; 24];
        want.extend([0.0; 24]);
        want.extend([1.0; 24]);
        want.extend([0.0; 24]);
        assert_eq!(cells, want);

        // A cell partly written shows how much.
        let cells = segment_cells(&[seg(0, 960, 490)], 960, 96);
        assert_eq!(cells[48], 1.0);
        assert_eq!(cells[49], 0.0);
        let cells = segment_cells(&[seg(0, 960, 485)], 960, 96);
        assert_eq!(cells[48], 0.5);

        // Unknown size reports [0, u64::MAX): clamped to the total, and no
        // cells at all without a total.
        let cells = segment_cells(&[seg(0, u64::MAX, 500)], 1000, 10);
        assert_eq!(cells, [1.0, 1.0, 1.0, 1.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0]);
        assert!(segment_cells(&[seg(0, u64::MAX, 500)], 0, 96).is_empty());
        assert!(segment_cells(&segments, 1920, 0).is_empty());
        // A done count past the segment's end stays in the segment.
        let cells = segment_cells(&[seg(0, 50, 1000), seg(50, 100, 0)], 100, 2);
        assert_eq!(cells, [1.0, 0.0]);
        // Fewer bytes than cells.
        let cells = segment_cells(&[seg(0, 4, 2)], 4, 8);
        assert_eq!(cells.len(), 8);
        assert_eq!(cells.iter().filter(|&&c| c == 1.0).count(), 4);
    }

    #[test]
    fn detail_vm_per_kind() {
        // A web download under way.
        let mut http = view(1, "big.iso", JobState::Downloading);
        http.total = Some(1920);
        http.downloaded = 960;
        http.speed_bps = 3_000_000;
        let mut d = detail(1);
        d.segments = vec![seg(0, 960, 480), seg(960, 1920, 480)];
        d.connections = 2;
        d.mirrors = vec!["https://m.example/big.iso".into()];
        d.checksum = Some("sha256:ab".into());
        d.referer = Some("https://example.org/".into());
        d.has_password = true;
        d.password_store = Some(PasswordStore::Memory);
        let vm = detail_vm(&http, &d, 0);
        assert!(vm.visible);
        assert_eq!(
            (vm.name.as_str(), vm.kind.as_str(), vm.source.as_str()),
            ("big.iso", "disc", "HTTP")
        );
        assert_eq!(vm.big.as_str(), "50%");
        assert_eq!(vm.size_line.as_str(), "960 B / 1.9 KB");
        assert!(vm.has_cells);
        let cells = items(&vm.cells);
        assert_eq!(cells.len(), CELLS);
        assert_eq!(cells.iter().filter(|&&c| c == 1.0).count(), 48);
        let stats: Vec<(String, String)> = items(&vm.stats)
            .into_iter()
            .map(|s| (s.key.to_string(), s.value.to_string()))
            .collect();
        let pair = |k: &str, v: &str| (k.to_string(), v.to_string());
        assert_eq!(
            stats,
            [
                pair("speed", "3.0 MB/s"),
                pair("connections", "2"),
                pair("segments", "2"),
                pair("mirrors", "1"),
                pair("checksum", "sha256:ab"),
                pair("referer", "https://example.org/"),
                pair("password", "memory"),
            ]
        );
        d.password_store = Some(PasswordStore::Keychain);
        let kept = items(&detail_vm(&http, &d, 0).stats);
        assert_eq!(
            (kept[6].key.as_str(), kept[6].value.as_str()),
            ("password", "keychain")
        );
        assert_eq!(vm.url.as_str(), "https://example.org/big.iso");
        assert_eq!(
            vm.folder.as_str(),
            PathBuf::from("/tmp").display().to_string()
        );
        assert_eq!(vm.row.id, 1);
        assert!(!vm.can_convert && !vm.needs_password && !vm.can_retry_cookies);
        assert!(!vm.can_choose_files && items(&vm.files).is_empty());

        // Unknown size: no cells, no percent.
        let mut unknown = http.clone();
        unknown.total = None;
        let mut d = detail(1);
        d.segments = vec![seg(0, u64::MAX, 500)];
        let vm = detail_vm(&unknown, &d, 0);
        assert!(!vm.has_cells && items(&vm.cells).is_empty());
        assert_eq!((vm.big.as_str(), vm.size_line.as_str()), ("", "960 B"));

        // Failed for want of a password; finished video file converts.
        let mut locked = view(2, "p.bin", JobState::Failed);
        locked.error = Some((ErrorKind::NeedsSignIn, String::new()));
        assert!(detail_vm(&locked, &detail(2), 0).needs_password);
        // A link's user name and password are never shown.
        let mut secret = view(8, "s.bin", JobState::Paused);
        secret.source_url = "https://ann:pw@example.org/s.bin?x=1".into();
        assert_eq!(
            detail_vm(&secret, &detail(8), 0).url.as_str(),
            "https://example.org/s.bin?x=1"
        );
        secret.source_url = "magnet:?xt=urn:btih:abc".into();
        assert_eq!(link_of(&secret), "magnet:?xt=urn:btih:abc");
        // One the URL parser refuses (a space in the host) loses its user
        // part all the same; an "@" later in the link stays.
        secret.source_url = "https://ann:p@ss@exa mple.org/a@b?c=d@e".into();
        assert!(url::Url::parse(&secret.source_url).is_err());
        assert_eq!(link_of(&secret), "https://exa mple.org/a@b?c=d@e");
        secret.source_url = "https://exa mple.org/a@b".into();
        assert_eq!(link_of(&secret), "https://exa mple.org/a@b");
        assert_eq!(strip_userinfo("ftp://u@host"), "ftp://host");
        assert_eq!(strip_userinfo("not a link@all"), "not a link@all");
        assert_eq!(
            scrub_links("https://ann:pw@h.org/a.bin: http://u@x answered 401\n"),
            "https://h.org/a.bin: http://x answered 401\n"
        );
        assert_eq!(
            scrub_links("mail me@home, no link"),
            "mail me@home, no link"
        );
        let mut done = view(3, "clip.mp4", JobState::Completed);
        done.downloaded = 1000;
        let vm = detail_vm(&done, &detail(3), 0);
        assert!(vm.can_convert && vm.has_cells);
        assert_eq!(vm.big.as_str(), "100%");
        assert!(items(&vm.cells).iter().all(|&c| c == 1.0));
        assert!(!detail_vm(&view(4, "a.zip", JobState::Completed), &detail(4), 0).can_convert);

        // A torrent: its files, overrides and "Choose files".
        let mut torrent = view(5, "linux", JobState::Downloading);
        torrent.kind = JobKind::Torrent;
        torrent.torrent = Some(TorrentView {
            peers: 3,
            uploaded: 2_000_000,
            upload_bps: 0,
            ratio: 0.5,
            files_selected: 1,
            files_total: 2,
            overrides: TorrentOverrides {
                stop_ratio: Some(Some(2.0)),
                stop_after_secs: Some(None),
            },
        });
        let mut d = detail(5);
        d.files = vec![
            TorrentFileProgress {
                index: 0,
                path: "a/1.bin".into(),
                size: 2000,
                done: 500,
                selected: true,
            },
            TorrentFileProgress {
                index: 1,
                path: "b.bin".into(),
                size: 0,
                done: 0,
                selected: false,
            },
        ];
        let vm = detail_vm(&torrent, &d, 0);
        assert_eq!(
            (vm.kind.as_str(), vm.source.as_str()),
            ("magnet", "Torrent")
        );
        assert!(vm.can_choose_files && !vm.has_cells);
        let files = items(&vm.files);
        assert_eq!(files.len(), 2);
        assert_eq!(
            (
                files[0].index,
                files[0].name.as_str(),
                files[0].size_text.as_str()
            ),
            (0, "a/1.bin", "2.0 KB")
        );
        assert_eq!((files[0].progress, files[0].selected), (0.25, true));
        assert_eq!((files[1].progress, files[1].selected), (1.0, false));
        assert_eq!(
            (vm.ratio_override.as_str(), vm.seed_time_override.as_str()),
            ("2", "never")
        );
        let keys: Vec<String> = items(&vm.stats).iter().map(|s| s.key.to_string()).collect();
        assert_eq!(keys, ["peers", "uploaded", "ratio", "files"]);
        assert_eq!(items(&vm.stats)[3].value.as_str(), "1/2");

        // A video that needs the browser's sign-in, at its download step.
        let mut video = view(6, "A Video", JobState::Failed);
        video.kind = JobKind::Media;
        video.error = Some((ErrorKind::BlockedByBotCheck, String::new()));
        video.media = Some(MediaView {
            phase: MediaPhase::Downloading,
            audio_only: false,
            live: false,
            entries_done: 0,
            entries_total: 0,
            live_elapsed_secs: None,
            starts_at: None,
            thumbnail: None,
            files: Vec::new(),
        });
        let vm = detail_vm(&video, &detail(6), 0);
        assert!(vm.can_retry_cookies && !vm.can_convert && !vm.needs_password);
        let steps: Vec<(String, String)> = items(&vm.steps)
            .into_iter()
            .map(|s| (s.key.to_string(), s.state.to_string()))
            .collect();
        let step = |k: &str, s: &str| (k.to_string(), s.to_string());
        assert_eq!(
            steps,
            [
                step("tools", "done"),
                step("download", "failed"),
                step("finish", "todo")
            ]
        );
        // Finished: every step done, its files listed, convertible.
        video.state = JobState::Completed;
        video.error = None;
        let mut d = detail(6);
        d.media_files = vec![PathBuf::from("/tmp/A Video.mp4")];
        // Without its files known, a finished video cannot be converted.
        assert!(!detail_vm(&video, &detail(6), 0).can_convert);
        let vm = detail_vm(&video, &d, 0);
        assert!(vm.can_convert && !vm.can_retry_cookies);
        assert!(items(&vm.steps).iter().all(|s| s.state == "done"));
        let files = items(&vm.files);
        assert_eq!(
            (files[0].index, files[0].name.as_str()),
            (-1, "A Video.mp4")
        );

        // A conversion counts time; its last progress (bytes) while the
        // view still says Millis shows it finishing, not 133%.
        let mut convert = view(7, "song (mp3).mp3", JobState::Downloading);
        convert.kind = JobKind::Convert;
        convert.progress_unit = ProgressUnit::Millis;
        convert.total = Some(3000);
        convert.downloaded = 1000;
        let vm = detail_vm(&convert, &detail(7), 0);
        assert_eq!(
            (vm.big.as_str(), vm.size_line.as_str()),
            ("33%", "0:01 / 0:03")
        );
        assert!(!vm.has_cells && items(&vm.stats).is_empty());
        // Its "link" is a file on disk: none shown.
        assert_eq!(vm.url.as_str(), "");
        convert.downloaded = 4000;
        let vm = detail_vm(&convert, &detail(7), 0);
        assert_eq!(
            (vm.big.as_str(), vm.size_line.as_str()),
            ("100%", "0:03 / 0:03")
        );
    }

    #[test]
    fn overrides_text_and_parse() {
        assert_eq!(ratio_text(None), "default");
        assert_eq!(ratio_text(Some(None)), "never");
        assert_eq!(ratio_text(Some(Some(1.0))), "1");
        assert_eq!(ratio_text(Some(Some(1.5))), "1.5");
        assert_eq!(secs_text(Some(Some(3600))), "3600");
        let current = TorrentOverrides {
            stop_ratio: Some(Some(2.0)),
            stop_after_secs: Some(Some(60)),
        };
        let o = with_override(&current, "ratio", "never").unwrap();
        assert_eq!(
            (o.stop_ratio, o.stop_after_secs),
            (Some(None), Some(Some(60)))
        );
        let o = with_override(&current, "seed-time", "default").unwrap();
        assert_eq!((o.stop_ratio, o.stop_after_secs), (Some(Some(2.0)), None));
        let o = with_override(&current, "seed-time", "86400").unwrap();
        assert_eq!(o.stop_after_secs, Some(Some(86400)));
        assert_eq!(
            with_override(&current, "ratio", "0.5").unwrap().stop_ratio,
            Some(Some(0.5))
        );
        assert!(with_override(&current, "ratio", "-1").is_none());
        assert!(with_override(&current, "ratio", "NaN").is_none());
        assert!(with_override(&current, "ratio", "lots").is_none());
        assert!(with_override(&current, "seed-time", "1.5").is_none());
        assert!(with_override(&current, "speed", "1").is_none());
    }
}
