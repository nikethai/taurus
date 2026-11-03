//! ffmpeg's arguments for a conversion and the names of its outputs.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::filename::unique_path;
use crate::media::ConvertPreset;

/// ffmpeg's arguments to convert `input` into `output` (the last argument).
/// `-n`: ffmpeg never overwrites an existing output; `-y` is never used.
/// `-progress pipe:1` makes it print `out_time_ms=` lines on stdout.
pub fn ffmpeg_args(preset: ConvertPreset, input: &Path, output: &Path) -> Vec<String> {
    let mut args: Vec<String> = ["-hide_banner", "-nostdin", "-n", "-i"]
        .map(String::from)
        .into();
    args.push(input.to_string_lossy().into_owned());
    let preset_args: &[&str] = match preset {
        ConvertPreset::Mp3 => &["-vn", "-c:a", "libmp3lame", "-q:a", "2"],
        ConvertPreset::SmallerMp4 => &[
            "-c:v",
            "libx264",
            "-crf",
            "28",
            "-preset",
            "veryfast",
            "-c:a",
            "aac",
            "-b:a",
            "128k",
            "-movflags",
            "+faststart",
        ],
    };
    args.extend(preset_args.iter().map(|a| (*a).to_owned()));
    args.extend(["-progress", "pipe:1"].map(String::from));
    args.push(output.to_string_lossy().into_owned());
    args
}

/// What the output's name adds to the source's stem, and its extension.
fn naming(preset: ConvertPreset) -> (&'static str, &'static str) {
    match preset {
        ConvertPreset::Mp3 => (" (mp3)", "mp3"),
        ConvertPreset::SmallerMp4 => (" (smaller)", "mp4"),
    }
}

/// The extension of the output of `preset`, without the dot.
pub(crate) fn extension(preset: ConvertPreset) -> &'static str {
    naming(preset).1
}

/// The output of converting `source`, next to it: `<stem> (mp3).mp3` or
/// `<stem> (smaller).mp4`, or the first of those with ` (1)`, ` (2)`, ...
/// before the extension that no file has and `reserved` (the names other
/// jobs hold in that folder) does not list. Never the source itself.
pub fn output_path(source: &Path, preset: ConvertPreset, reserved: &HashSet<String>) -> PathBuf {
    let (suffix, ext) = naming(preset);
    let stem = source
        .file_stem()
        .map_or_else(|| "converted".into(), |s| s.to_string_lossy());
    let dir = source.parent().unwrap_or_else(|| Path::new("."));
    unique_path(dir, &format!("{stem}{suffix}.{ext}"), reserved)
}
