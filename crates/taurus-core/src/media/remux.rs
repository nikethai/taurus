//! What a stopped live recording left behind: yt-dlp's partial files (one
//! per stream: `--live-from-start` downloads the video and the audio side
//! by side), joined by ffmpeg into one playable MP4.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use tokio::process::Command;

use crate::errors::{CoreError, ErrorKind};
use crate::tools::tree::TreeChild;

use super::run::{Tracker, read_tail};

/// The partial files a stopped recording left: nothing if the run never
/// reported progress; else those it reported for the current video that
/// still exist, else the `.part` and `.ts` files in `dir` changed since
/// `since` whose names carry the video's id as `[<id>]` (yt-dlp's output
/// name), so another job's file in the same folder is never taken.
pub(crate) fn recorded_parts(dir: &Path, tracker: &Tracker, since: SystemTime) -> Vec<PathBuf> {
    if !tracker.progressed {
        return Vec::new();
    }
    let tag = tracker.video_id.as_deref().map(|id| format!("[{id}]"));
    let current = |path: &Path| {
        tag.as_deref().is_none_or(|tag| {
            path.file_name()
                .is_some_and(|n| n.to_string_lossy().contains(tag))
        })
    };
    let reported: Vec<PathBuf> = tracker
        .parts
        .iter()
        .filter(|p| p.is_file() && current(p))
        .cloned()
        .collect();
    if !reported.is_empty() {
        return reported;
    }
    let Some(tag) = tag else {
        return Vec::new();
    };
    let since = since.checked_sub(Duration::from_secs(2)).unwrap_or(since);
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
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
    found
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
/// worked. Returns the MP4; on failure nothing of it is left. Dropping the
/// future stops ffmpeg; the caller then deletes `out` (see `RemuxPlan`).
pub(crate) async fn remux_parts(ffmpeg: &Path, plan: &RemuxPlan) -> Result<PathBuf, CoreError> {
    let RemuxPlan { parts, out } = plan;
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
    for part in parts {
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
}

impl RemuxPlan {
    /// `None` without parts.
    pub(crate) fn new(parts: Vec<PathBuf>) -> Option<RemuxPlan> {
        if parts.is_empty() {
            return None;
        }
        let out = free_name(&parts);
        Some(RemuxPlan { parts, out })
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
        let plan = RemuxPlan::new(vec![part.clone()]).unwrap();
        assert_eq!(plan.out, tmp.path().join("Live [a].mp4"));
        std::fs::write(tmp.path().join("Live [a].mp4"), b"mine").unwrap();
        std::fs::write(tmp.path().join("Live [a] (2).mp4"), b"mine too").unwrap();
        let plan = RemuxPlan::new(vec![part]).unwrap();
        assert_eq!(plan.out, tmp.path().join("Live [a] (3).mp4"));
        assert!(RemuxPlan::new(Vec::new()).is_none());
    }

    #[test]
    fn the_recorded_parts_are_found() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let before = SystemTime::now() - Duration::from_secs(60);
        let mut t = Tracker::new(dir, MediaPhase::WaitingForStream);
        t.apply("TAURUS-ENTRY 0 1 \"abc\" \"Live\"");
        let video = dir.join("Live [abc].f137.mp4.part");
        let audio = dir.join("Live [abc].f140.m4a.part");
        std::fs::write(&video, b"x").unwrap();
        std::fs::write(&audio, b"x").unwrap();
        // Nothing recorded yet (no progress): nothing is taken.
        assert!(recorded_parts(dir, &t, before).is_empty());
        t.apply(&progress(
            serde_json::json!({"status": "downloading", "downloaded_bytes": 9}),
        ));
        // Another job's partial files in the same folder are never taken.
        std::fs::write(dir.join("Other [zzz].mp4.part"), b"x").unwrap();
        std::fs::write(dir.join("other.ts"), b"x").unwrap();
        std::fs::write(dir.join("notes [abc].txt"), b"x").unwrap();
        assert_eq!(
            recorded_parts(dir, &t, before),
            [video.clone(), audio.clone()]
        );
        // Without the video's id, the folder is not searched at all.
        let mut unknown = t.clone();
        unknown.video_id = None;
        assert!(recorded_parts(dir, &unknown, before).is_empty());
        // Parts older than the run are another run's.
        let later = SystemTime::now() + Duration::from_secs(60);
        assert!(recorded_parts(dir, &t, later).is_empty());
        // Those the run reported win; a reported one that is gone, or
        // another video's, does not count.
        t.parts = vec![
            audio.clone(),
            dir.join("gone [abc].part"),
            dir.join("Other [zzz].mp4.part"),
            video.clone(),
        ];
        assert_eq!(recorded_parts(dir, &t, later), [audio, video]);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_failed_remux_keeps_the_parts() {
        let tmp = tempfile::tempdir().unwrap();
        let part = tmp.path().join("x.mp4.part");
        std::fs::write(&part, b"data").unwrap();
        let plan = RemuxPlan::new(vec![part.clone()]).unwrap();
        let err = remux_parts(Path::new("false"), &plan).await.unwrap_err();
        assert_eq!(err.kind, ErrorKind::ToolFailed);
        assert!(part.is_file());
        // A tool that "works" but writes no output file fails too.
        assert!(remux_parts(Path::new("true"), &plan).await.is_err());
        assert!(part.is_file());
    }
}
