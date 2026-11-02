use std::path::PathBuf;
use std::time::SystemTime;

use super::*;
use crate::media::{Container, SubtitleChoice};

fn options(quality: &str, selector: &str, audio_only: bool) -> MediaOptions {
    MediaOptions {
        quality: quality.into(),
        selector: selector.into(),
        audio_only,
        ..MediaOptions::default()
    }
}

fn strings(args: &[OsString]) -> Vec<String> {
    args.iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect()
}

#[test]
fn video_args_in_order() {
    let dest = Path::new("/dl");
    let args = strings(&download_args(
        &options("best", "bv*+ba/b", false),
        dest,
        "https://v",
        None,
        true,
    ));
    let template = dest.join(SINGLE_TEMPLATE).to_string_lossy().into_owned();
    let expected = [
        "-f",
        "bv*+ba/b",
        "--merge-output-format",
        "mp4",
        "--embed-thumbnail",
        "--embed-chapters",
        "--embed-metadata",
        "--no-playlist",
        "-o",
        &template,
        "--progress",
        "--progress-template",
        "download:TAURUS-PROGRESS %(progress)j",
        "--progress-template",
        "postprocess:TAURUS-PP %(progress.postprocessor)s %(progress.status)s",
        "--print",
        "after_move:TAURUS-FILE %(filepath)j",
        "--print",
        "before_dl:TAURUS-ENTRY %(playlist_index|0)s %(n_entries|1)s %(id)j %(title)j %(filename)j",
        "--",
        "https://v",
    ];
    assert_eq!(args, expected);
}

#[test]
fn audio_playlist_live_and_conversions() {
    let dest = Path::new("/dl");
    let audio = strings(&download_args(
        &options("audio-best", "ba/b", true),
        dest,
        "u",
        None,
        true,
    ));
    assert_eq!(&audio[..3], ["-f", "ba/b", "-x"]);
    assert!(!audio.iter().any(|a| a == "--merge-output-format"));

    let small = strings(&download_args(
        &options("audio-small", "ba/b", true),
        dest,
        "u",
        None,
        true,
    ));
    assert_eq!(
        &small[..7],
        [
            "-f",
            "ba/b",
            "-x",
            "--audio-format",
            "mp3",
            "--audio-quality",
            "5"
        ]
    );
    // The MP3 preset wins over the option's own audio arguments, and makes
    // even a video quality audio.
    for quality in [
        options("audio-small", "ba/b", true),
        options("best", "bv*+ba/b", false),
    ] {
        let mp3 = MediaOptions {
            convert: Some(ConvertPreset::Mp3),
            ..quality
        };
        let args = strings(&download_args(&mp3, dest, "u", None, true));
        assert_eq!(
            &args[2..7],
            ["-x", "--audio-format", "mp3", "--audio-quality", "2"]
        );
        assert!(!args.iter().any(|a| a == "--merge-output-format"));
    }
    let smaller = MediaOptions {
        convert: Some(ConvertPreset::SmallerMp4),
        ..options("720p", "bv*[height<=720]+ba/b[height<=720]", false)
    };
    let args = strings(&download_args(&smaller, dest, "u", None, true));
    assert_eq!(
        &args[2..8],
        [
            "--merge-output-format",
            "mp4",
            "--recode-video",
            "mp4",
            "--postprocessor-args",
            "VideoConvertor:-crf 28 -preset veryfast"
        ]
    );

    let live_list = MediaOptions {
        playlist: true,
        live_from_start: true,
        ..options("best", "bv*+ba/b", false)
    };
    let args = strings(&download_args(&live_list, dest, "u", None, true));
    assert!(!args.iter().any(|a| a == "--no-playlist"));
    let at = args.iter().position(|a| a == "--live-from-start").unwrap();
    assert_eq!(
        &args[at..at + 3],
        ["--live-from-start", "--wait-for-video", "30-600"]
    );
    let at = args.iter().position(|a| a == "-o").unwrap();
    assert_eq!(args[at + 1], dest.join(PLAYLIST_TEMPLATE).to_string_lossy());
    // Where the site cannot record from the start: from now on, still
    // waiting for an upcoming stream.
    let args = strings(&download_args(&live_list, dest, "u", None, false));
    assert!(!args.iter().any(|a| a == "--live-from-start"), "{args:?}");
    let at = args.iter().position(|a| a == "--wait-for-video").unwrap();
    assert_eq!(args[at + 1], "30-600");
    // `from_start` means nothing for a video that is not live.
    let video = options("best", "bv*+ba/b", false);
    assert_eq!(
        download_args(&video, dest, "u", None, true),
        download_args(&video, dest, "u", None, false)
    );
}

#[test]
fn a_speed_limit_becomes_limit_rate() {
    let dest = Path::new("/dl");
    let best = options("best", "bv*+ba/b", false);
    let limited = strings(&download_args(&best, dest, "u", Some(250_000), true));
    let at = limited.iter().position(|a| a == "--limit-rate").unwrap();
    assert_eq!(limited[at + 1], "250000");
    assert!(at < limited.iter().position(|a| a == "--").unwrap());
    let fastest = strings(&download_args(&best, dest, "u", None, true));
    assert!(!fastest.iter().any(|a| a == "--limit-rate"));
}

fn progress(json: serde_json::Value) -> String {
    format!("{PROGRESS}{json}")
}

#[test]
fn tracker_follows_a_video_and_its_audio() {
    let dest = Path::new("/dl");
    let mut t = Tracker::new(dest, MediaPhase::Downloading);
    assert_eq!(t.apply("[info] something else"), Change::default());
    let c = t.apply(&progress(serde_json::json!({
        "status": "downloading", "downloaded_bytes": 40, "total_bytes": 100,
        "speed": 12.5, "filename": "v.f137.mp4", "tmpfilename": "v.f137.mp4.part"
    })));
    assert_eq!(c.progress, Some((40, Some(100), 12)));
    assert!(!c.view, "already Downloading");
    assert_eq!(t.parts, [dest.join("v.f137.mp4.part")]);
    t.apply(&progress(serde_json::json!({
        "status": "finished", "downloaded_bytes": 100, "total_bytes": 100,
        "filename": "v.f137.mp4"
    })));
    // The audio stream: its size adds to the video's.
    let c = t.apply(&progress(serde_json::json!({
        "status": "downloading", "downloaded_bytes": 5, "total_bytes_estimate": 20.0,
        "speed": null, "filename": "v.f140.m4a"
    })));
    assert_eq!(c.progress, Some((105, Some(120), 0)));
    let c = t.apply(&format!("{POSTPROCESS}Merger started"));
    assert!(c.view);
    assert_eq!(t.phase, MediaPhase::Merging);
    assert_eq!(
        t.apply(&format!("{POSTPROCESS}Merger finished")),
        Change::default()
    );
    assert_eq!(
        t.apply(&format!("{POSTPROCESS}EmbedThumbnail started")),
        Change::default()
    );
    // yt-dlp's real keys (`pp_key()` drops the `FFmpeg` prefix).
    for (key, phase) in [
        ("ExtractAudio", MediaPhase::Converting),
        ("Merger", MediaPhase::Merging),
        ("VideoConvertor", MediaPhase::Converting),
        ("Merger", MediaPhase::Merging),
        ("VideoRemuxer", MediaPhase::Converting),
        ("FFmpegMerger", MediaPhase::Merging),
        ("FFmpegExtractAudio", MediaPhase::Converting),
    ] {
        let c = t.apply(&format!("{POSTPROCESS}{key} processing"));
        assert!(c.view, "{key}");
        assert_eq!(t.phase, phase, "{key}");
    }
    let c = t.apply(&format!("{FILE}\"/elsewhere/v.mp4\""));
    assert!(c.view);
    t.apply(&format!("{FILE}\"v2.mp4\""));
    assert_eq!(
        t.files,
        [PathBuf::from("/elsewhere/v.mp4"), dest.join("v2.mp4")]
    );
    assert_eq!(t.entries_done, 1);
    t.finish();
    assert_eq!((t.entries_done, t.entries_total), (1, 1));
}

#[test]
fn tracker_counts_playlist_entries() {
    let mut t = Tracker::new(Path::new("/dl"), MediaPhase::Downloading);
    assert!(t.apply(&format!("{ENTRY}1 4 \"id 1\" \"One\"")).view);
    assert_eq!(t.video_id.as_deref(), Some("id 1"));
    assert_eq!((t.entries_done, t.entries_total), (0, 4));
    let c = t.apply(&progress(serde_json::json!({
        "status": "downloading", "downloaded_bytes": 50, "total_bytes": 100
    })));
    // Half of the first of four entries of about 100 bytes each.
    assert_eq!(c.progress, Some((50, Some(400), 0)));
    t.apply(&progress(serde_json::json!({
        "status": "finished", "downloaded_bytes": 100, "total_bytes": 100
    })));
    t.apply(&format!("{FILE}\"L/001 - One.mp4\""));
    t.apply(&format!("{ENTRY}2 4 \"b2\" \"Two\""));
    assert_eq!(t.video_id.as_deref(), Some("b2"));
    t.apply(&format!("{ENTRY}2 4 \"b2\" \"Two\""));
    assert_eq!(t.entries_done, 1);
    let c = t.apply(&progress(serde_json::json!({
        "status": "downloading", "downloaded_bytes": 30
    })));
    // No size known: the bytes so far, without a total.
    assert_eq!(c.progress, Some((130, None, 0)));
    // Lines that are not quite ours are ignored.
    assert_eq!(t.apply(&format!("{ENTRY}x 4 \"Bad\"")), Change::default());
    assert_eq!(t.apply(&format!("{PROGRESS}not json")), Change::default());
    assert_eq!(t.apply(&format!("{FILE}not json")), Change::default());
}

#[test]
fn a_live_stream_waits_until_its_first_progress() {
    let mut t = Tracker::new(Path::new("/dl"), MediaPhase::WaitingForStream);
    assert!(!t.progressed);
    let c = t.apply(&progress(serde_json::json!({
        "status": "downloading", "downloaded_bytes": 4096, "fragment_index": 2
    })));
    assert!(c.view);
    assert!(t.progressed);
    assert_eq!(t.phase, MediaPhase::Downloading);
    assert_eq!(c.progress, Some((4096, None, 0)));
}

#[test]
fn an_announced_entry_names_its_partial_file() {
    let dest = Path::new("/dl");
    let mut t = Tracker::new(dest, MediaPhase::WaitingForStream);
    let entry = format!("{ENTRY}0 1 \"live1\" \"Live\" \"/dl/Live [live1].mp4\"");
    let c = t.apply(&entry);
    assert!(c.view && c.partials, "{c:?}");
    assert_eq!(c.progress, None);
    // A live stream begins with its entry (yt-dlp records it through
    // ffmpeg and prints no progress).
    assert!(t.started && !t.progressed);
    assert_eq!(t.phase, MediaPhase::Downloading);
    assert_eq!(t.video_id.as_deref(), Some("live1"));
    let part = dest.join("Live [live1].mp4.part");
    assert_eq!(t.parts, std::slice::from_ref(&part));
    assert_eq!(t.partials, [part]);
    assert_eq!(t.entry_files, [dest.join("Live [live1].mp4")]);
    // The same entry again: nothing new to store.
    assert!(!t.apply(&entry).partials);
    assert_eq!(t.entry_files.len(), 1);

    // Not live: an entry starts nothing. `NA` (no such field) is no file.
    let mut v = Tracker::new(dest, MediaPhase::Downloading);
    let c = v.apply(&format!("{ENTRY}1 2 \"a\" \"A\" \"NA\""));
    assert!(c.view && !c.partials);
    assert!(v.entry_files.is_empty() && v.parts.is_empty());
    v.apply(&format!("{ENTRY}1 2 \"a\" \"A\" \"L/001 - A [a].avi\""));
    assert_eq!(v.entry_files, [dest.join("L/001 - A [a].avi")]);
    assert!(!v.started);
    assert_eq!(v.phase, MediaPhase::Downloading);
}

#[test]
fn files_are_kept_without_the_thumbnails_they_could_not_hold() {
    let tmp = tempfile::tempdir().unwrap();
    let dest = tmp.path();
    let write = |name: &str| {
        let path = dest.join(name);
        std::fs::write(&path, b"x").unwrap();
        path
    };
    let (one, one_jpg) = (write("One [a].avi"), write("One [a].jpg"));
    let (two, two_png) = (write("Two.x [b].webm"), write("Two.x [b].png"));
    let mut t = Tracker::new(dest, MediaPhase::Downloading);
    for (index, name) in [
        (1, "One [a].avi"),
        (2, "Two.x [b].webm"),
        (3, "Gone [c].avi"),
    ] {
        t.apply(&format!(
            "{ENTRY}{index} 3 \"x\" \"x\" {}",
            serde_json::json!(dest.join(name))
        ));
    }
    t.apply(&format!("{FILE}{}", serde_json::json!(two)));
    // Thumbnails older than the run are not its own.
    let mut early = t.clone();
    early.keep_without_thumbnails(SystemTime::now() + Duration::from_secs(60));
    assert!(one_jpg.is_file() && two_png.is_file());

    t.keep_without_thumbnails(SystemTime::now() - Duration::from_secs(60));
    // Finished in announced order; one already reported is not repeated;
    // one that is not there is not.
    assert_eq!(t.files, [two.clone(), one.clone()]);
    assert!(!one_jpg.exists() && !two_png.exists());
    assert!(one.is_file() && two.is_file());
    assert!(t.partials.is_empty(), "{:?}", t.partials);
}

#[test]
fn printed_paths_get_one_spelling() {
    let dest = Path::new("/dl");
    assert_eq!(
        local_path(dest, "L/001 - a.mp4"),
        PathBuf::from("/dl/L/001 - a.mp4")
    );
    assert_eq!(
        local_path(dest, "/else/./b.mp4"),
        PathBuf::from("/else/b.mp4")
    );
    #[cfg(windows)]
    {
        let dest = Path::new(r"\\?\C:\dl");
        assert_eq!(
            local_path(dest, "L/001 - a.mp4").as_os_str(),
            r"C:\dl\L\001 - a.mp4"
        );
        assert_eq!(
            local_path(Path::new(r"C:\dl"), r"C:/dl\x.mp4").as_os_str(),
            r"C:\dl\x.mp4"
        );
    }
}

#[cfg(unix)]
mod process {
    use super::*;
    use crate::tools::tree::test_support::gone_soon;

    /// A shell that ignores Ctrl+C and has a grandchild that does too.
    fn stubborn() -> Command {
        let mut command = Command::new("sh");
        command
            .args(["-c", "trap '' INT; sleep 300 & echo $!; wait"])
            .stdin(std::process::Stdio::null());
        command
    }

    #[tokio::test]
    async fn an_ignored_interrupt_ends_in_a_kill_of_the_tree() {
        let pause = CancellationToken::new();
        let finish = CancellationToken::new();
        let mut grandchild = None;
        let stopper = pause.clone();
        let started = std::time::Instant::now();
        let ended = run_process(
            stubborn(),
            &pause,
            &finish,
            Duration::from_millis(300),
            |out| {
                if let Output::Line(line) = out {
                    grandchild = line.trim().parse::<libc::pid_t>().ok();
                    stopper.cancel();
                }
            },
        )
        .await
        .unwrap();
        let elapsed = started.elapsed();
        // First, so that a failure leaves nothing running.
        let gone = gone_soon(grandchild.unwrap()).await;
        assert_eq!(ended.stopped, Some(Stop::Pause));
        assert!(ended.status.is_none(), "killed, not exited: {ended:?}");
        assert!(!ended.success());
        assert!(elapsed >= Duration::from_millis(300));
        assert!(gone, "the grandchild survived");
    }

    /// A shell that ends on Ctrl+C, with a grandchild that ignores it: as
    /// yt-dlp's bootloader ending while something it started does not.
    fn leader_that_ends_first() -> Command {
        let mut command = Command::new("sh");
        command
            .args(["-c", "trap '' INT; sleep 300 & trap - INT; echo $!; wait"])
            .stdin(std::process::Stdio::null());
        // A test run started in the background of a non-interactive shell
        // inherits SIGINT as ignored, which `trap -` could not undo.
        // SAFETY: only `signal`, which is async-signal-safe, runs after fork.
        unsafe {
            command.pre_exec(|| {
                libc::signal(libc::SIGINT, libc::SIG_DFL);
                Ok(())
            });
        }
        command
    }

    #[tokio::test]
    async fn a_leader_that_ends_on_the_interrupt_takes_its_tree_along() {
        use std::os::unix::process::ExitStatusExt;
        let pause = CancellationToken::new();
        let never = CancellationToken::new();
        let mut grandchild = None;
        let stopper = pause.clone();
        let ended = run_process(
            leader_that_ends_first(),
            &pause,
            &never,
            Duration::from_secs(30),
            |out| {
                if let Output::Line(line) = out {
                    grandchild = line.trim().parse::<libc::pid_t>().ok();
                    stopper.cancel();
                }
            },
        )
        .await
        .unwrap();
        let gone = gone_soon(grandchild.unwrap()).await;
        assert_eq!(ended.stopped, Some(Stop::Pause));
        // It ended by itself, long before the grace was over.
        let status = ended.status.expect("exited, not killed");
        // Linux shells die of the signal itself; macOS's exits with 128 + 2.
        assert!(
            status.signal() == Some(libc::SIGINT) || status.code().is_some_and(|c| c != 0),
            "{ended:?}"
        );
        assert!(gone, "the grandchild survived");
    }

    #[tokio::test]
    async fn a_run_reports_its_lines_status_and_stderr() {
        let mut command = Command::new("sh");
        command
            .args(["-c", "echo one; echo two; echo 'ERROR: bad' >&2; exit 3"])
            .stdin(std::process::Stdio::null());
        let never = CancellationToken::new();
        let mut lines = Vec::new();
        let ended = run_process(command, &never, &never, Duration::from_secs(1), |out| {
            if let Output::Line(l) = out {
                lines.push(l.to_owned());
            }
        })
        .await
        .unwrap();
        assert_eq!(lines, ["one", "two"]);
        assert_eq!(ended.status.and_then(|s| s.code()), Some(3));
        assert_eq!(ended.stopped, None);
        assert_eq!(ended.stderr.trim(), "ERROR: bad");
    }

    #[tokio::test]
    async fn a_quiet_run_still_ticks_every_second() {
        // As yt-dlp recording a live stream: nothing printed for a while.
        let mut command = Command::new("sh");
        command
            .args(["-c", "sleep 4; echo done"])
            .stdin(std::process::Stdio::null());
        let never = CancellationToken::new();
        let mut seen = Vec::new();
        run_process(command, &never, &never, Duration::from_secs(1), |out| {
            seen.push(match out {
                Output::Line(l) => l.to_owned(),
                Output::Tick => "tick".to_owned(),
            });
        })
        .await
        .unwrap();
        // At least two whole seconds of ticks, however slow the start.
        let ticks = seen.iter().filter(|s| *s == "tick").count();
        assert!(ticks >= 2, "{seen:?}");
        // A tick can still come between the last line and the end being
        // seen; no line is lost.
        let lines: Vec<_> = seen.iter().filter(|s| *s != "tick").collect();
        assert_eq!(lines, ["done"], "{seen:?}");
    }

    #[tokio::test]
    async fn bad_output_lines_do_not_stop_the_reading() {
        // Bytes that are not UTF-8, then a line far longer than kept, then
        // more than a pipe holds after them: all of it is read.
        let script = "printf 'a\\377\\376b\\n'; head -c 300000 /dev/zero | tr '\\0' x; echo; \
                      i=0; while [ $i -lt 2000 ]; do echo \"line $i ................................\"; \
                      i=$((i+1)); done; printf 'last'";
        let mut command = Command::new("sh");
        command
            .args(["-c", script])
            .stdin(std::process::Stdio::null());
        let never = CancellationToken::new();
        let mut lines = Vec::new();
        let ended = run_process(command, &never, &never, Duration::from_secs(1), |out| {
            if let Output::Line(l) = out {
                lines.push(l.to_owned());
            }
        })
        .await
        .unwrap();
        assert!(ended.success());
        assert_eq!(lines[0], "a\u{fffd}\u{fffd}b");
        assert_eq!(lines[1].len(), MAX_LINE);
        assert_eq!(lines[2], format!("line 0 {}", ".".repeat(32)));
        assert_eq!(lines.len(), 2 + 2000 + 1);
        assert_eq!(lines.last().unwrap(), "last");
    }

    #[tokio::test]
    async fn our_lines_on_stderr_are_read_too() {
        // yt-dlp prints the postprocessing template to stderr under the
        // `--quiet` that `--print` implies.
        let mut command = Command::new("sh");
        command
            .args([
                "-c",
                "echo 'TAURUS-PROGRESS {}'; echo 'TAURUS-PP ExtractAudio started' >&2; \
                 echo 'ERROR: bad' >&2; exit 1",
            ])
            .stdin(std::process::Stdio::null());
        let never = CancellationToken::new();
        let mut lines = Vec::new();
        let ended = run_process(command, &never, &never, Duration::from_secs(1), |out| {
            if let Output::Line(l) = out {
                lines.push(l.to_owned());
            }
        })
        .await
        .unwrap();
        lines.sort();
        assert_eq!(
            lines,
            ["TAURUS-PP ExtractAudio started", "TAURUS-PROGRESS {}"]
        );
        assert_eq!(ended.stderr.trim(), "ERROR: bad");
    }
}

#[test]
fn tracker_records_the_jobs_partial_files() {
    let tmp = tempfile::tempdir().unwrap();
    let dest = tmp.path();
    let at = |name: &str| local_path(dest, name);
    let writes = |tmp: &str, file: &str| {
        progress(serde_json::json!({
            "status": "downloading", "downloaded_bytes": 1,
            "tmpfilename": tmp, "filename": file,
        }))
    };
    let mut t = Tracker::new(dest, MediaPhase::Downloading);
    // A file already downloaded (reported without a `tmpfilename`) is not
    // this run's.
    let c = t.apply(&progress(serde_json::json!({
        "status": "finished", "downloaded_bytes": 1, "filename": "old.mp4"
    })));
    assert!(!c.partials && t.partials.is_empty());
    let video = writes("L/1 [a].f137.mp4.part", "L/1 [a].f137.mp4");
    assert!(t.apply(&video).partials);
    assert!(!t.apply(&video).partials, "known already");
    assert_eq!(
        t.partials,
        [at("L/1 [a].f137.mp4.part"), at("L/1 [a].f137.mp4")]
    );
    // The entry is finished (merged): yt-dlp removed its streams.
    assert!(t.apply(&format!("{FILE}\"L/1 [a].mp4\"")).partials);
    assert!(t.partials.is_empty());
    // A single stream's file is the finished one: it leaves the list.
    std::fs::create_dir_all(dest.join("L")).unwrap();
    std::fs::write(at("L/2 [b].mp4.part"), b"x").unwrap();
    std::fs::write(at("L/2 [b].mp4"), b"x").unwrap();
    t.apply(&writes("L/2 [b].mp4.part", "L/2 [b].mp4"));
    assert!(t.apply(&format!("{FILE}\"L/2 [b].mp4\"")).partials);
    assert_eq!(t.partials, [at("L/2 [b].mp4.part")]);
}

#[test]
fn cookies_subtitles_and_container_args() {
    let dest = Path::new("/dl");
    let base = options("best", "bv*+ba/b", false);
    let with = MediaOptions {
        cookies_from_browser: Some("firefox:work".into()),
        subtitles: Some(SubtitleChoice {
            languages: vec!["en".into(), "vi".into()],
            auto_generated: true,
            embed: true,
        }),
        container: Some(Container::Mkv),
        ..base.clone()
    };
    let args = strings(&download_args(&with, dest, "u", None, true));
    assert_eq!(
        &args[..11],
        [
            "-f",
            "bv*+ba/b",
            "--cookies-from-browser",
            "firefox:work",
            "--write-subs",
            "--write-auto-subs",
            "--sub-langs",
            "en,vi",
            "--embed-subs",
            "--merge-output-format",
            "mkv"
        ]
    );
    // Without the auto-generated and embed choices; "all" wins.
    let plain = MediaOptions {
        subtitles: Some(SubtitleChoice {
            languages: vec!["en".into(), "all".into()],
            auto_generated: false,
            embed: false,
        }),
        container: Some(Container::Webm),
        ..base.clone()
    };
    let args = strings(&download_args(&plain, dest, "u", None, true));
    assert_eq!(
        &args[..7],
        [
            "-f",
            "bv*+ba/b",
            "--write-subs",
            "--sub-langs",
            "all,-live_chat",
            "--merge-output-format",
            "webm"
        ]
    );
    // WebM takes only WebVTT: its subtitles are not embedded.
    let webm = MediaOptions {
        subtitles: Some(SubtitleChoice {
            languages: vec!["en".into()],
            auto_generated: false,
            embed: true,
        }),
        container: Some(Container::Webm),
        ..base.clone()
    };
    let args = strings(&download_args(&webm, dest, "u", None, true));
    assert!(args.iter().any(|a| a == "--write-subs"));
    assert!(!args.iter().any(|a| a == "--embed-subs"));
    // Audio ignores the container and never embeds; sidecar subtitles and
    // cookies stay.
    let audio = MediaOptions {
        audio_only: true,
        ..with
    };
    let args = strings(&download_args(&audio, dest, "u", None, true));
    assert!(
        !args
            .iter()
            .any(|a| a == "--merge-output-format" || a == "--embed-subs")
    );
    assert!(args.iter().any(|a| a == "--write-subs"));
    assert!(args.iter().any(|a| a == "--cookies-from-browser"));
}
