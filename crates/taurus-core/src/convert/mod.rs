//! Converting a finished file with ffmpeg: the job's stored description
//! ([`ConvertSpec`]), what a caller can convert ([`ConvertSource`]), the
//! arguments and output names (`args`) and reading ffmpeg and ffprobe
//! (`probe`). The engine's convert job (`engine::convert`) decides when to
//! run it and what to keep.
//!
//! A conversion never overwrites and never touches its source: ffmpeg runs
//! with `-n`, writes to a private temporary file, and the result is moved
//! to its `<stem> (mp3).mp3` / `<stem> (smaller).mp4` name only once that
//! name was created exclusively (see `engine::convert_job`).

use std::path::PathBuf;

use crate::media::ConvertPreset;
use crate::store::JobId;

mod args;
pub(crate) mod probe;

pub(crate) use args::extension;
pub use args::{ffmpeg_args, output_path};

/// What to convert.
#[derive(Clone, Debug, PartialEq)]
pub enum ConvertSource {
    /// A finished job's file. A finished playlist makes one conversion per
    /// finished file (`Engine::convert_many`).
    Job(JobId),
    /// Any local file that is not being downloaded.
    Path(PathBuf),
}

/// What a convert job converts, as stored (JSON in `convert_source`).
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ConvertSpec {
    /// The file to convert (absolute); never modified.
    pub source: PathBuf,
    pub preset: ConvertPreset,
    /// The temporary file ffmpeg writes to, recorded before ffmpeg starts
    /// and only once the folder holding it was created by the job: it is
    /// this job's, so the job (also after a restart, or when it is removed)
    /// may delete it, and that folder.
    #[serde(default)]
    pub partial: Option<PathBuf>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::path::Path;

    fn none() -> HashSet<String> {
        HashSet::new()
    }

    #[test]
    fn args_never_overwrite() {
        for preset in [ConvertPreset::Mp3, ConvertPreset::SmallerMp4] {
            let args = ffmpeg_args(preset, Path::new("/d/in.webm"), Path::new("/d/out"));
            assert!(args.iter().any(|a| a == "-n"), "{args:?}");
            assert!(!args.iter().any(|a| a == "-y"), "{args:?}");
            assert_eq!(args.last().map(String::as_str), Some("/d/out"));
        }
    }

    #[test]
    fn args_are_the_presets() {
        let (input, output) = (Path::new("/d/in.webm"), Path::new("/d/out.mp3"));
        assert_eq!(
            ffmpeg_args(ConvertPreset::Mp3, input, output).join(" "),
            "-hide_banner -nostdin -n -i /d/in.webm -vn -c:a libmp3lame -q:a 2 \
             -progress pipe:1 /d/out.mp3"
        );
        assert_eq!(
            ffmpeg_args(ConvertPreset::SmallerMp4, input, output).join(" "),
            "-hide_banner -nostdin -n -i /d/in.webm -c:v libx264 -crf 28 -preset veryfast \
             -c:a aac -b:a 128k -movflags +faststart -progress pipe:1 /d/out.mp3"
        );
    }

    #[test]
    fn output_names() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("song.webm");
        std::fs::write(&source, b"x").unwrap();
        assert_eq!(
            output_path(&source, ConvertPreset::Mp3, &none()),
            dir.path().join("song (mp3).mp3")
        );
        assert_eq!(
            output_path(&source, ConvertPreset::SmallerMp4, &none()),
            dir.path().join("song (smaller).mp4")
        );
        // An existing file, or a name another job holds, is never taken.
        std::fs::write(dir.path().join("song (mp3).mp3"), b"mine").unwrap();
        assert_eq!(
            output_path(&source, ConvertPreset::Mp3, &none()),
            dir.path().join("song (mp3) (1).mp3")
        );
        let held = HashSet::from(["song (mp3) (1).mp3".to_owned()]);
        assert_eq!(
            output_path(&source, ConvertPreset::Mp3, &held),
            dir.path().join("song (mp3) (2).mp3")
        );
        // Only the last extension goes: the stem keeps its dots.
        let dotted = dir.path().join("a.b.c.mkv");
        assert_eq!(
            output_path(&dotted, ConvertPreset::SmallerMp4, &none()),
            dir.path().join("a.b.c (smaller).mp4")
        );
    }

    #[test]
    fn the_output_is_never_the_source() {
        let dir = tempfile::tempdir().unwrap();
        // A source that already looks like an output.
        let source = dir.path().join("song (mp3).mp3");
        std::fs::write(&source, b"x").unwrap();
        let out = output_path(&source, ConvertPreset::Mp3, &none());
        assert_ne!(out, source);
        assert_eq!(out, dir.path().join("song (mp3) (mp3).mp3"));
    }

    #[test]
    fn progress_lines() {
        use probe::progress_ms;
        assert_eq!(progress_ms("out_time_ms=1500000"), Some(1500));
        assert_eq!(progress_ms("out_time_us=2000000"), Some(2000));
        assert_eq!(progress_ms("out_time_ms=-9000"), Some(0));
        assert_eq!(progress_ms("out_time_ms=N/A"), None);
        assert_eq!(progress_ms("progress=continue"), None);
        assert_eq!(progress_ms("out_time=00:00:01.5"), None);
    }
}
