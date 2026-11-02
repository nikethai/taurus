//! Job views turned into what the screens show.

use crate::format;
use crate::{RowVm, SummaryVm};
use taurus_core::media::MediaPhase;
use taurus_core::{JobKind, JobState, JobView};

const DISC: &[&str] = &["iso", "img", "dmg"];
const MEDIA: &[&str] = &[
    "mp4", "mkv", "avi", "mov", "webm", "m4v", "wmv", "flv", "mpg", "mpeg",
];
const MUSIC: &[&str] = &["mp3", "flac", "wav", "ogg", "m4a", "aac", "opus", "wma"];
const ARCHIVE: &[&str] = &["zip", "rar", "7z", "tar", "gz", "tgz", "bz2", "xz", "zst"];
const IMAGE: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "webp", "bmp", "svg", "tif", "tiff", "heic",
];

/// Icon family of a file: "disc", "media", "music", "archive", "image",
/// "magnet" (any torrent job) or "file". A media job's file is "media"
/// unless its extension says otherwise.
pub fn kind_for(file_name: &str, job_kind: JobKind) -> &'static str {
    if job_kind == JobKind::Torrent {
        return "magnet";
    }
    let fallback = if job_kind == JobKind::Media {
        "media"
    } else {
        "file"
    };
    let Some((_, ext)) = file_name.rsplit_once('.') else {
        return fallback;
    };
    let ext = ext.to_ascii_lowercase();
    let ext = ext.as_str();
    for (list, kind) in [
        (DISC, "disc"),
        (MEDIA, "media"),
        (MUSIC, "music"),
        (ARCHIVE, "archive"),
        (IMAGE, "image"),
    ] {
        if list.contains(&ext) {
            return kind;
        }
    }
    fallback
}

/// Icon family of a row: a media job is "live" while it records (or waits
/// to record), "music" when only its audio is kept, else "media"; other
/// jobs as `kind_for` says.
pub fn row_kind(view: &JobView) -> &'static str {
    match &view.media {
        Some(m)
            if view.state == JobState::Recording
                || (m.live && view.state != JobState::Completed) =>
        {
            "live"
        }
        Some(m) if m.audio_only => "music",
        Some(_) => "media",
        None => kind_for(&view.file_name, view.kind),
    }
}

fn state_name(state: JobState) -> &'static str {
    match state {
        JobState::Queued => "queued",
        JobState::Downloading => "downloading",
        JobState::Paused => "paused",
        JobState::Waiting => "waiting",
        JobState::Seeding => "seeding",
        JobState::Failed => "failed",
        JobState::Completed => "done",
        JobState::Recording => "recording",
    }
}

fn phase_name(phase: MediaPhase) -> &'static str {
    match phase {
        MediaPhase::GettingTools => "getting-tools",
        MediaPhase::WaitingForStream => "waiting-for-stream",
        MediaPhase::Downloading => "downloading",
        MediaPhase::Merging => "merging",
        MediaPhase::Converting => "converting",
    }
}

fn clamp_i32(n: i64) -> i32 {
    n.clamp(0, i32::MAX as i64) as i32
}

/// One list row. `now_unix` is the current time in Unix seconds; finish
/// times are shown in the computer's local time.
pub fn row_of(view: &JobView, now_unix: i64) -> RowVm {
    row_in(view, now_unix, &chrono::Local)
}

/// `row_of` with finish times in the time zone `tz`.
pub fn row_in<Tz: chrono::TimeZone>(view: &JobView, now_unix: i64, tz: &Tz) -> RowVm {
    let progress = if view.state == JobState::Completed {
        1.0
    } else {
        match view.total {
            Some(t) if t > 0 => (view.downloaded as f64 / t as f64).clamp(0.0, 1.0) as f32,
            _ => -1.0,
        }
    };
    let eta = match (view.state, view.total) {
        (JobState::Downloading, Some(t)) => {
            format::eta(t.saturating_sub(view.downloaded), view.speed_bps)
        }
        _ => format::Eta::Unknown,
    };
    let (eta_kind, eta_value) = format::eta_fields(eta);
    let retry_seconds = view
        .retry_at
        .map_or(0, |at| clamp_i32(at.saturating_sub(now_unix)));
    let finished = match (view.state, view.finished_at) {
        (JobState::Completed, Some(at)) => format::moment(at, now_unix, tz),
        _ => None,
    };
    RowVm {
        id: view.id.0.clamp(0, i32::MAX as i64) as i32,
        name: view.file_name.as_str().into(),
        kind: row_kind(view).into(),
        state: state_name(view.state).into(),
        progress,
        done_text: format::bytes(view.downloaded).into(),
        total_text: view.total.map(format::bytes).unwrap_or_default().into(),
        eta_kind: eta_kind.into(),
        eta_value,
        retry_seconds,
        error_kind: view.error.as_ref().map_or("", |(k, _)| k.as_str()).into(),
        ratio_text: view
            .torrent
            .as_ref()
            .map(|t| format::ratio(t.ratio))
            .unwrap_or_default()
            .into(),
        // Month 0: no finish time to show.
        finished_month: finished.map_or(0, |m| m.month as i32),
        finished_day: finished.map_or(0, |m| m.day as i32),
        finished_hour: finished.map_or(0, |m| m.hour as i32),
        finished_minute: finished.map_or(0, |m| m.minute as i32),
        finished_today: finished.is_some_and(|m| m.same_day),
        peers: view
            .torrent
            .as_ref()
            .map_or(0, |t| t.peers.min(i32::MAX as u32) as i32),
        phase: view
            .media
            .as_ref()
            .map_or("", |m| phase_name(m.phase))
            .into(),
        entries_done: view
            .media
            .as_ref()
            .map_or(0, |m| m.entries_done.min(i32::MAX as u32) as i32),
        entries_total: view
            .media
            .as_ref()
            .map_or(0, |m| m.entries_total.min(i32::MAX as u32) as i32),
        live_elapsed: view
            .media
            .as_ref()
            .and_then(|m| m.live_elapsed_secs)
            .map(format::duration)
            .unwrap_or_default()
            .into(),
    }
}

/// (Downloading tab, Finished tab). Finished holds the Completed jobs,
/// newest first (by `finished_at`, then by id). Everything else, Seeding,
/// Failed and Paused included, is in Downloading in the order given:
/// `JobView` has no queue position, so the caller's order (the engine's
/// snapshot is by id, which is the order jobs were added) is kept.
pub fn split_tabs(views: &[JobView]) -> (Vec<JobView>, Vec<JobView>) {
    let (mut finished, active): (Vec<JobView>, Vec<JobView>) = views
        .iter()
        .cloned()
        .partition(|v| v.state == JobState::Completed);
    finished.sort_by(|a, b| {
        b.finished_at
            .cmp(&a.finished_at)
            .then_with(|| b.id.cmp(&a.id))
    });
    (active, finished)
}

/// Footer numbers. `downloading` is the size of the Downloading tab and
/// `finished` the size of the Finished tab. The overall time left is the
/// bytes left over the summed speed of the Downloading jobs; Unknown when
/// none runs or one of them has no known size.
pub fn summary(views: &[JobView]) -> SummaryVm {
    let finished = views
        .iter()
        .filter(|v| v.state == JobState::Completed)
        .count();
    let open: Vec<&JobView> = views
        .iter()
        .filter(|v| v.state != JobState::Completed)
        .collect();
    let running: Vec<&JobView> = open
        .iter()
        .copied()
        .filter(|v| v.state == JobState::Downloading)
        .collect();
    let eta = if running.iter().any(|v| v.total.is_none()) {
        format::Eta::Unknown
    } else {
        let left: u64 = running
            .iter()
            .map(|v| v.total.unwrap_or(0).saturating_sub(v.downloaded))
            .sum();
        let speed: u64 = running.iter().map(|v| v.speed_bps).sum();
        format::eta(left, speed)
    };
    let (eta_kind, eta_value) = format::eta_fields(eta);
    SummaryVm {
        downloading: open.len() as i32,
        finished: finished as i32,
        eta_kind: eta_kind.into(),
        eta_value,
        all_paused: !open.is_empty() && open.iter().all(|v| v.state == JobState::Paused),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::view;
    use taurus_core::{ErrorKind, TorrentView};

    #[test]
    fn kind_for_cases() {
        assert_eq!(kind_for("a.iso", JobKind::Http), "disc");
        assert_eq!(kind_for("x.MKV", JobKind::Http), "media");
        assert_eq!(kind_for("y.flac", JobKind::Http), "music");
        assert_eq!(kind_for("z.tar.gz", JobKind::Http), "archive");
        assert_eq!(kind_for("p.png", JobKind::Http), "image");
        assert_eq!(kind_for("movie.mkv", JobKind::Torrent), "magnet");
        assert_eq!(kind_for("README", JobKind::Http), "file");
        assert_eq!(kind_for("notes.txt", JobKind::Http), "file");
    }

    #[test]
    fn media_rows() {
        let now = 1_793_541_900;
        let media = |state, audio_only, live| {
            let mut v = view(1, "A Video", state);
            v.kind = JobKind::Media;
            v.media = Some(taurus_core::MediaView {
                phase: taurus_core::media::MediaPhase::Downloading,
                audio_only,
                live,
                entries_done: 0,
                entries_total: 0,
                live_elapsed_secs: None,
                starts_at: None,
                thumbnail: None,
                files: Vec::new(),
            });
            v
        };
        let row = |v: &JobView| row_of(v, now);
        assert_eq!(
            row(&media(JobState::Downloading, false, false)).kind,
            "media"
        );
        assert_eq!(row(&media(JobState::Queued, true, false)).kind, "music");
        let recording = row(&media(JobState::Recording, false, true));
        assert_eq!(
            (recording.kind.as_str(), recording.state.as_str()),
            ("live", "recording")
        );
        assert_eq!(row(&media(JobState::Queued, false, true)).kind, "live");
        assert_eq!(row(&media(JobState::Completed, false, true)).kind, "media");
        let mut v = media(JobState::Recording, false, true);
        let m = v.media.as_mut().unwrap();
        m.live_elapsed_secs = Some(65);
        m.entries_done = 2;
        m.entries_total = 5;
        m.phase = taurus_core::media::MediaPhase::Merging;
        let r = row(&v);
        assert_eq!(
            (
                r.phase.as_str(),
                r.live_elapsed.as_str(),
                r.entries_done,
                r.entries_total
            ),
            ("merging", "1:05", 2, 5)
        );
        let plain = row(&view(2, "a.bin", JobState::Downloading));
        assert_eq!(
            (plain.phase.as_str(), plain.live_elapsed.as_str()),
            ("", "")
        );
        assert_eq!(kind_for("A Video", JobKind::Media), "media");
        assert_eq!(kind_for("song.mp3", JobKind::Media), "music");
    }

    #[test]
    fn row_states() {
        let now = 1_793_541_900;
        let expect = [
            (JobState::Queued, "queued"),
            (JobState::Downloading, "downloading"),
            (JobState::Paused, "paused"),
            (JobState::Waiting, "waiting"),
            (JobState::Seeding, "seeding"),
            (JobState::Failed, "failed"),
            (JobState::Completed, "done"),
            (JobState::Recording, "recording"),
        ];
        for (state, name) in expect {
            assert_eq!(row_of(&view(1, "a.bin", state), now).state, name);
        }

        let mut waiting = view(2, "a.bin", JobState::Waiting);
        waiting.retry_at = Some(now + 65);
        assert_eq!(row_of(&waiting, now).retry_seconds, 65);
        waiting.retry_at = Some(now - 5);
        assert_eq!(row_of(&waiting, now).retry_seconds, 0);

        let mut failed = view(3, "a.bin", JobState::Failed);
        failed.error = Some((ErrorKind::LinkExpired, "gone".into()));
        assert_eq!(row_of(&failed, now).error_kind, "link_expired");
        assert_eq!(
            row_of(&view(3, "a.bin", JobState::Paused), now).error_kind,
            ""
        );

        let mut unknown = view(4, "a.bin", JobState::Downloading);
        unknown.total = None;
        unknown.downloaded = 500;
        unknown.speed_bps = 100;
        let r = row_of(&unknown, now);
        assert_eq!(r.progress, -1.0);
        assert_eq!(r.eta_kind, "unknown");
        assert_eq!(r.total_text, "");
        assert_eq!(r.done_text, "500 B");

        let mut run = view(5, "a.bin", JobState::Downloading);
        run.downloaded = 250;
        run.speed_bps = 5;
        let r = row_of(&run, now);
        assert_eq!(r.progress, 0.25);
        assert_eq!((r.eta_kind.as_str(), r.eta_value), ("minutes", 3));
        assert_eq!(r.total_text, "1.0 KB");

        let mut done = view(6, "a.bin", JobState::Completed);
        done.downloaded = 1000;
        done.finished_at = Some(now - 600);
        let r = row_in(&done, now, &chrono::Utc);
        assert_eq!(r.progress, 1.0);
        assert_eq!(
            (r.finished_today, r.finished_hour, r.finished_minute),
            (true, 13, 55)
        );
        assert_eq!((r.finished_month, r.finished_day), (11, 1));
        done.finished_at = Some(now - 7 * 86_400);
        let r = row_in(&done, now, &chrono::Utc);
        assert!(!r.finished_today);
        assert_eq!((r.finished_month, r.finished_day), (10, 25));
        // Not Completed (or no time): nothing to show.
        done.state = JobState::Seeding;
        assert_eq!(row_in(&done, now, &chrono::Utc).finished_month, 0);
    }

    #[test]
    fn row_torrent_fields() {
        let mut t = view(7, "linux", JobState::Seeding);
        t.kind = JobKind::Torrent;
        t.torrent = Some(TorrentView {
            peers: 4,
            uploaded: 420,
            upload_bps: 0,
            ratio: 0.42,
            files_selected: 1,
            files_total: 1,
            overrides: Default::default(),
        });
        let r = row_of(&t, 0);
        assert_eq!(
            (r.kind.as_str(), r.ratio_text.as_str(), r.peers),
            ("magnet", "0.42", 4)
        );
        assert_eq!(row_of(&view(8, "a", JobState::Queued), 0).ratio_text, "");
    }

    #[test]
    fn tabs_and_order() {
        let mut a = view(1, "a", JobState::Completed);
        a.finished_at = Some(100);
        let mut b = view(2, "b", JobState::Completed);
        b.finished_at = Some(300);
        let mut c = view(3, "c", JobState::Completed);
        c.finished_at = Some(300);
        let d = view(4, "d", JobState::Completed); // no finish time: last
        let views = vec![
            view(9, "p", JobState::Paused),
            a,
            b,
            view(5, "s", JobState::Seeding),
            c,
            d,
            view(7, "f", JobState::Failed),
            view(6, "q", JobState::Queued),
        ];
        let (active, finished) = split_tabs(&views);
        let ids = |v: &[JobView]| v.iter().map(|x| x.id.0).collect::<Vec<_>>();
        assert_eq!(ids(&active), [9, 5, 7, 6]);
        assert_eq!(ids(&finished), [3, 2, 1, 4]);
    }

    #[test]
    fn summary_counts() {
        let mut run = view(1, "a", JobState::Downloading);
        run.speed_bps = 10;
        run.downloaded = 400; // 600 left at 10 B/s = 60 s
        let mut run2 = view(2, "b", JobState::Downloading);
        run2.speed_bps = 10; // 1000 left; together 1600 / 20 = 80 s
        let done = view(3, "c", JobState::Completed);
        let s = summary(&[run, run2, done.clone(), view(4, "d", JobState::Paused)]);
        assert_eq!((s.downloading, s.finished), (3, 1));
        assert_eq!((s.eta_kind.as_str(), s.eta_value), ("minutes", 2));
        assert!(!s.all_paused);

        let s = summary(&[view(1, "a", JobState::Paused), done]);
        assert!(s.all_paused);
        assert_eq!(s.eta_kind, "unknown");
        assert!(!summary(&[]).all_paused);
    }
}
