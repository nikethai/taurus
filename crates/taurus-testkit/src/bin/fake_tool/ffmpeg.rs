//! The fake ffmpeg and ffprobe (see `taurus_testkit::tools` for the contract).

use std::env;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::Path;
use std::thread;
use std::time::Duration;

/// `ffmpeg [-y] -i <in> [-i <in> ...] [-map N ...] -c copy <out>` writes
/// the inputs, one after the other, to `<out>` (one input: a plain copy;
/// several: what a real remux joining a video and an audio stream would
/// hold, for a test's purposes). With `-progress pipe:1` it is a
/// conversion instead (see `convert`). Anything else exits 0 without
/// doing anything.
pub fn ffmpeg(args: &[String]) -> i32 {
    if args
        .windows(2)
        .any(|w| w[0] == "-progress" && w[1] == "pipe:1")
    {
        return convert(args);
    }
    let inputs: Vec<&String> = args
        .windows(2)
        .filter(|w| w[0] == "-i")
        .map(|w| &w[1])
        .collect();
    let copy = args
        .windows(2)
        .any(|w| (w[0] == "-c" || w[0] == "-codec") && w[1] == "copy");
    let (false, true, Some(output)) = (inputs.is_empty(), copy, args.last()) else {
        return 0;
    };
    if inputs.contains(&output) {
        return 0;
    }
    if Path::new(output).exists() && !args.iter().any(|a| a == "-y") {
        eprintln!("File '{output}' already exists. Exiting.");
        return 1;
    }
    let mut joined = Vec::new();
    for input in &inputs {
        match fs::read(input) {
            Ok(bytes) => joined.extend_from_slice(&bytes),
            Err(e) => {
                eprintln!("{input}: {e}");
                return 1;
            }
        }
    }
    match fs::write(output, joined) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("{output}: {e}");
            1
        }
    }
}

/// A conversion (`... -i <in> ... -progress pipe:1 <out>`): refuses an
/// existing `<out>` without `-y` (exit 1, as ffmpeg's `-n` does); else
/// creates `<out>` at once with the first half of `<in>`, prints
/// `out_time_ms=` lines (microseconds) for 1 s, 2 s and 3 s of media time,
/// `FAKE_FFMPEG_STEP_MS` apart (default 0), and ends by copying the rest of
/// `<in>` to `<out>`. With `FAKE_FFMPEG_FAIL=1` it exits 1 after writing the
/// first half, printing no progress. A killed run leaves the half file.
fn convert(args: &[String]) -> i32 {
    let input = args.windows(2).find(|w| w[0] == "-i").map(|w| &w[1]);
    let (Some(input), Some(output)) = (input, args.last()) else {
        return 0;
    };
    if Path::new(output).exists() && !args.iter().any(|a| a == "-y") {
        eprintln!("File '{output}' already exists. Exiting.");
        return 1;
    }
    let bytes = match fs::read(input) {
        Ok(bytes) => bytes,
        Err(e) => {
            eprintln!("{input}: {e}");
            return 1;
        }
    };
    let (first, rest) = bytes.split_at(bytes.len() / 2);
    let mut file = match File::create(output).and_then(|mut f| f.write_all(first).map(|()| f)) {
        Ok(file) => file,
        Err(e) => {
            eprintln!("{output}: {e}");
            return 1;
        }
    };
    if env::var("FAKE_FFMPEG_FAIL").is_ok_and(|v| v == "1") {
        eprintln!("fake ffmpeg: {output}: conversion failed halfway");
        return 1;
    }
    let step = env::var("FAKE_FFMPEG_STEP_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let mut out = io::stdout().lock();
    for seconds in 1..=3u64 {
        let _ = writeln!(out, "out_time_ms={}", seconds * 1_000_000);
        let _ = writeln!(out, "progress=continue");
        let _ = out.flush();
        thread::sleep(Duration::from_millis(step));
    }
    let _ = writeln!(out, "progress=end");
    let _ = out.flush();
    match file.write_all(rest).and_then(|()| file.flush()) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("{output}: {e}");
            1
        }
    }
}

/// `ffprobe ... -show_entries format=duration ...` prints
/// `FAKE_FFPROBE_DURATION` seconds (default `3.000000`); anything else
/// exits 0 silently.
pub fn ffprobe(args: &[String]) -> i32 {
    if args.iter().any(|a| a == "-show_entries") {
        let seconds = env::var("FAKE_FFPROBE_DURATION").unwrap_or_else(|_| "3.000000".to_owned());
        println!("{seconds}");
    }
    0
}
