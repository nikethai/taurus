//! What a stopped live recording left behind: yt-dlp's partial files (one
//! per stream: `--live-from-start` downloads the video and the audio side
//! by side), joined by ffmpeg into one playable MP4.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use tokio::process::Command;

use crate::errors::{CoreError, ErrorKind};
use crate::tools::tree::TreeChild;

use super::process::read_tail;
use super::run::Tracker;
use super::tracker::written_since;

/// The partial files a stopped recording left, and whether they are the
/// run's own: nothing if the download never began (`Tracker::started`);
/// else those yt-dlp announced or reported for the current entry that
/// still exist (its own: `Tracker::parts`); else the `.part` and `.ts`
/// files in `dir` changed since `since` whose names carry the video's id
/// as `[<id>]` (yt-dlp's output name). Those are found by their name only,
/// so they could be another job's: they are joined into the recording but
/// never deleted (see `RemuxPlan::owned`).
pub(crate) fn recorded_parts(
    dir: &Path,
    tracker: &Tracker,
    since: SystemTime,
) -> (Vec<PathBuf>, bool) {
    if !tracker.started {
        return (Vec::new(), true);
    }
    let reported: Vec<PathBuf> = tracker
        .parts
        .iter()
        .filter(|p| p.is_file())
        .cloned()
        .collect();
    if !reported.is_empty() {
        return (reported, true);
    }
    let Some(tag) = tracker.video_id.as_deref().map(|id| format!("[{id}]")) else {
        return (Vec::new(), false);
    };
    let since = since.checked_sub(Duration::from_secs(2)).unwrap_or(since);
    let Ok(entries) = std::fs::read_dir(dir) else {
        return (Vec::new(), false);
    };
    let mut found: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .filter(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let lower = name.to_ascii_lowercase();
            name.contains(&tag) && (lower.ends_with(".part") || lower.ends_with(".ts"))
        })
        .filter(|e| {
            e.metadata().is_ok_and(|meta| {
                meta.is_file() && meta.modified().is_ok_and(|modified| modified >= since)
            })
        })
        .map(|e| e.path())
        .collect();
    found.sort();
    (found, false)
}

/// The recording yt-dlp was finalizing when it was killed (a long one
/// outlasts the grace): it had renamed its part to the announced entry
/// file, `<filename>`, but not yet reported it (TAURUS-FILE comes after
/// its postprocessors rewrite the file, and the first of them, FixupM3u8,
/// turns the MPEG-TS it recorded into an MP4). So the last announced entry
/// file that was written since the run started (`since`) is this job's
/// recording, possibly still MPEG-TS under an `.mp4` name: it is renamed
/// back to its part, `<filename>.part`, for `remux_parts` to make it
/// playable under its own name. `None` if there is none (or the rename
/// failed).
pub(crate) fn unmoved_recording(tracker: &Tracker, since: SystemTime) -> Option<PathBuf> {
    if !tracker.started {
        return None;
    }
    let since = since.checked_sub(Duration::from_secs(2)).unwrap_or(since);
    let file = tracker
        .entry_files
        .iter()
        .rfind(|f| written_since(f, since))?;
    let mut part = file.clone().into_os_string();
    part.push(".part");
    let part = PathBuf::from(part);
    match std::fs::rename(file, &part) {
        Ok(()) => Some(part),
        Err(e) => {
            tracing::warn!("could not take {}: {e}", file.display());
            None
        }
    }
}

/// Where a partial recording goes once remuxed: its name without `.part`,
/// its own extension and yt-dlp's format suffix (`.f137`), as `.mp4`.
pub(crate) fn remuxed_name(part: &Path) -> PathBuf {
    let name = part
        .file_name()
        .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
    let name = name.strip_suffix(".part").unwrap_or(&name);
    let stem = Path::new(name)
        .file_stem()
        .map_or_else(|| name.to_owned(), |s| s.to_string_lossy().into_owned());
    // yt-dlp names a single format's file `<title> [<id>].f<format>.<ext>`.
    let stem = match stem.rsplit_once('.') {
        Some((base, format))
            if format.len() > 1 && format.starts_with('f') && !format.contains([' ', ']']) =>
        {
            base.to_owned()
        }
        _ => stem,
    };
    part.with_file_name(format!("{stem}.mp4"))
}

/// `remuxed_name` of the first part, or, if a file of that name exists
/// already, the first free `<name> (2).mp4`, `(3)`, … beside it.
fn free_name(parts: &[PathBuf]) -> PathBuf {
    let wanted = remuxed_name(&parts[0]);
    if !wanted.exists() {
        return wanted;
    }
    let stem = wanted
        .file_stem()
        .map_or_else(String::new, |s| s.to_string_lossy().into_owned());
    (2..)
        .map(|n| wanted.with_file_name(format!("{stem} ({n}).mp4")))
        .find(|candidate| !candidate.exists())
        .expect("some number is free")
}

/// Copies the streams of a recording's partial files into one playable
/// MP4 with ffmpeg (`ffmpeg -y -i <part> [-i <part> -map 0 -map 1 …] -c
/// copy <final>.mp4`, every stream of every part), named after them
/// without overwriting another file, and deletes the parts only if that
/// worked and they are the run's own (`RemuxPlan::owned`). Returns the MP4;
/// on failure nothing of it is left. Dropping the future stops ffmpeg; the
/// caller then deletes `out` (see `RemuxPlan`).
pub(crate) async fn remux_parts(ffmpeg: &Path, plan: &RemuxPlan) -> Result<PathBuf, CoreError> {
    let RemuxPlan { parts, out, owned } = plan;
    let mut command = Command::new(ffmpeg);
    command.arg("-y");
    for part in parts {
        command.arg("-i").arg(part);
    }
    if parts.len() > 1 {
        for index in 0..parts.len() {
            command.arg("-map").arg(index.to_string());
        }
    }
    command
        .args(["-c", "copy"])
        .arg(out)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped());
    let mut run = TreeChild::spawn(&mut command)
        .await
        .map_err(|e| tool_failed(format!("could not run ffmpeg: {e}")))?;
    let mut stderr = run.child.stderr.take().expect("stderr is piped");
    let (tail, status) = tokio::join!(read_tail(&mut stderr, 16 * 1024), run.child.wait());
    let status = status.map_err(|e| tool_failed(format!("could not wait for ffmpeg: {e}")))?;
    if !status.success() || !out.is_file() {
        let _ = std::fs::remove_file(out);
        let tail = String::from_utf8_lossy(&tail);
        let last = tail.lines().rfind(|l| !l.trim().is_empty()).unwrap_or("");
        return Err(tool_failed(format!(
            "ffmpeg could not finish the recording ({status}): {last}"
        )));
    }
    for part in parts.iter().filter(|_| *owned) {
        if let Err(e) = std::fs::remove_file(part) {
            tracing::warn!("could not remove {}: {e}", part.display());
        }
    }
    Ok(out.clone())
}

/// The parts to remux and the free name they go to.
#[derive(Debug)]
pub(crate) struct RemuxPlan {
    pub parts: Vec<PathBuf>,
    pub out: PathBuf,
    /// The parts are the ones the run reported writing: deleted once
    /// joined. Parts found only by their name are kept.
    pub owned: bool,
}

impl RemuxPlan {
    /// `None` without parts.
    pub(crate) fn new((parts, owned): (Vec<PathBuf>, bool)) -> Option<RemuxPlan> {
        if parts.is_empty() {
            return None;
        }
        let out = free_name(&parts);
        Some(RemuxPlan { parts, out, owned })
    }
}

fn tool_failed(detail: String) -> CoreError {
    CoreError {
        kind: ErrorKind::ToolFailed,
        detail,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::run::MediaPhase;

    fn progress(json: serde_json::Value) -> String {
        format!("TAURUS-PROGRESS {json}")
    }

    #[test]
    fn remuxed_names() {
        for (part, mp4) in [
            ("/d/live.mp4.part", "/d/live.mp4"),
            ("/d/Stream [x].ts", "/d/Stream [x].mp4"),
            ("/d/Stream [x].f299.mp4.part", "/d/Stream [x].mp4"),
            ("/d/Stream [x].f140.m4a.part", "/d/Stream [x].mp4"),
            (
                "/d/My Report.final [x].mp4.part",
                "/d/My Report.final [x].mp4",
            ),
            ("/d/odd.part", "/d/odd.mp4"),
        ] {
            assert_eq!(remuxed_name(Path::new(part)), PathBuf::from(mp4), "{part}");
        }
    }

    #[test]
    fn an_existing_file_is_not_taken() {
        let tmp = tempfile::tempdir().unwrap();
        let part = tmp.path().join("Live [a].f137.mp4.part");
        std::fs::write(&part, b"x").unwrap();
        let plan = RemuxPlan::new((vec![part.clone()], true)).unwrap();
        assert_eq!(plan.out, tmp.path().join("Live [a].mp4"));
        std::fs::write(tmp.path().join("Live [a].mp4"), b"mine").unwrap();
        std::fs::write(tmp.path().join("Live [a] (2).mp4"), b"mine too").unwrap();
        let plan = RemuxPlan::new((vec![part], true)).unwrap();
        assert_eq!(plan.out, tmp.path().join("Live [a] (3).mp4"));
        assert!(RemuxPlan::new((Vec::new(), true)).is_none());
    }

    #[test]
    fn the_recorded_parts_are_found() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let before = SystemTime::now() - Duration::from_secs(60);
        let mut t = Tracker::new(dir, MediaPhase::WaitingForStream);
        let video = dir.join("Live [abc].f137.mp4.part");
        let audio = dir.join("Live [abc].f140.m4a.part");
        std::fs::write(&video, b"x").unwrap();
        std::fs::write(&audio, b"x").unwrap();
        // The stream has not begun: nothing is taken.
        assert!(recorded_parts(dir, &t, before).0.is_empty());
        // It begins (an entry announced without its file name).
        t.apply("TAURUS-ENTRY 0 1 \"abc\" \"Live\"");
        assert!(t.started);
        // Another job's partial files in the same folder are never taken.
        std::fs::write(dir.join("Other [zzz].mp4.part"), b"x").unwrap();
        std::fs::write(dir.join("other.ts"), b"x").unwrap();
        std::fs::write(dir.join("notes [abc].txt"), b"x").unwrap();
        // Found by name only: not the run's own, so never deleted.
        assert_eq!(
            recorded_parts(dir, &t, before),
            (vec![video.clone(), audio.clone()], false)
        );
        // Without the video's id, the folder is not searched at all.
        let mut unknown = t.clone();
        unknown.video_id = None;
        assert!(recorded_parts(dir, &unknown, before).0.is_empty());
        // Parts older than the run are another run's.
        let later = SystemTime::now() + Duration::from_secs(60);
        assert!(recorded_parts(dir, &t, later).0.is_empty());
        // Those the run reported win; a reported one that is gone does not
        // count.
        t.apply(&progress(serde_json::json!({
            "status": "downloading", "downloaded_bytes": 9, "tmpfilename": audio,
        })));
        t.apply(&progress(serde_json::json!({
            "status": "downloading", "downloaded_bytes": 9, "tmpfilename": dir.join("gone [abc].part"),
        })));
        t.apply(&progress(serde_json::json!({
            "status": "downloading", "downloaded_bytes": 9, "tmpfilename": video,
        })));
        assert_eq!(
            recorded_parts(dir, &t, later),
            (vec![audio.clone(), video.clone()], true)
        );
        // A new entry starts over, with the file yt-dlp announced for it
        // (a live stream recorded through ffmpeg reports nothing else).
        let file = dir.join("Next ⧸ one [a⧸b].mp4");
        t.apply(&format!(
            "TAURUS-ENTRY 0 1 \"a/b\" \"Next / one\" {}",
            serde_json::json!(file)
        ));
        let part = dir.join("Next ⧸ one [a⧸b].mp4.part");
        assert_eq!(t.parts, std::slice::from_ref(&part));
        assert!(
            recorded_parts(dir, &t, later).0.is_empty(),
            "not written yet"
        );
        std::fs::write(&part, b"ts").unwrap();
        assert_eq!(recorded_parts(dir, &t, later), (vec![part], true));
    }

    #[test]
    fn a_recording_renamed_but_not_reported_is_taken_back() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let before = SystemTime::now() - Duration::from_secs(60);
        let file = dir.join("Live [a].mp4");
        std::fs::write(&file, b"ts").unwrap();
        let mut t = Tracker::new(dir, MediaPhase::WaitingForStream);
        // Not begun: nothing of it.
        assert!(unmoved_recording(&t, before).is_none());
        t.apply(&format!(
            "TAURUS-ENTRY 0 1 \"a\" \"Live\" {}",
            serde_json::json!(file)
        ));
        // Older than the run: not this run's.
        let later = SystemTime::now() + Duration::from_secs(60);
        assert!(unmoved_recording(&t, later).is_none());
        assert!(file.is_file());
        let part = unmoved_recording(&t, before).unwrap();
        assert_eq!(part, dir.join("Live [a].mp4.part"));
        assert!(part.is_file() && !file.exists());
        // Remuxed back under its own name.
        let plan = RemuxPlan::new((vec![part], true)).unwrap();
        assert_eq!(plan.out, file);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_failed_remux_keeps_the_parts() {
        let tmp = tempfile::tempdir().unwrap();
        let part = tmp.path().join("x.mp4.part");
        std::fs::write(&part, b"data").unwrap();
        let plan = RemuxPlan::new((vec![part.clone()], true)).unwrap();
        let err = remux_parts(Path::new("false"), &plan).await.unwrap_err();
        assert_eq!(err.kind, ErrorKind::ToolFailed);
        assert!(part.is_file());
        // A tool that "works" but writes no output file fails too.
        assert!(remux_parts(Path::new("true"), &plan).await.is_err());
        assert!(part.is_file());
    }
}
