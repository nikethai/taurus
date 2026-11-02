use super::*;

fn options(quality: &str, selector: &str, audio_only: bool) -> MediaOptions {
    MediaOptions {
        quality: quality.into(),
        selector: selector.into(),
        audio_only,
        convert: None,
        playlist: false,
        live_from_start: false,
        partial_files: Vec::new(),
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
        "before_dl:TAURUS-ENTRY %(playlist_index|0)s %(n_entries|1)s %(id)j %(title)j",
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
    ));
    assert_eq!(&audio[..3], ["-f", "ba/b", "-x"]);
    assert!(!audio.iter().any(|a| a == "--merge-output-format"));

    let small = strings(&download_args(
        &options("audio-small", "ba/b", true),
        dest,
        "u",
        None,
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
        let args = strings(&download_args(&mp3, dest, "u", None));
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
    let args = strings(&download_args(&smaller, dest, "u", None));
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
    let args = strings(&download_args(&live_list, dest, "u", None));
    assert!(!args.iter().any(|a| a == "--no-playlist"));
    let at = args.iter().position(|a| a == "--live-from-start").unwrap();
    assert_eq!(
        &args[at..at + 3],
        ["--live-from-start", "--wait-for-video", "30-600"]
    );
    let at = args.iter().position(|a| a == "-o").unwrap();
    assert_eq!(args[at + 1], dest.join(PLAYLIST_TEMPLATE).to_string_lossy());
}

#[test]
fn a_speed_limit_becomes_limit_rate() {
    let dest = Path::new("/dl");
    let best = options("best", "bv*+ba/b", false);
    let limited = strings(&download_args(&best, dest, "u", Some(250_000)));
    let at = limited.iter().position(|a| a == "--limit-rate").unwrap();
    assert_eq!(limited[at + 1], "250000");
    assert!(at < limited.iter().position(|a| a == "--").unwrap());
    let fastest = strings(&download_args(&best, dest, "u", None));
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

    /// A shell that ignores Ctrl+C and has a grandchild that does too.
    fn stubborn() -> Command {
        let mut command = Command::new("sh");
        command
            .args(["-c", "trap '' INT; sleep 300 & echo $!; wait"])
            .stdin(std::process::Stdio::null());
        command
    }

    fn alive(pid: libc::pid_t) -> bool {
        // SAFETY: signal 0 only checks that the process exists.
        if unsafe { libc::kill(pid, 0) } != 0 {
            return false;
        }
        // A zombie nobody reaped (an orphan in a container) does not count.
        std::fs::read_to_string(format!("/proc/{pid}/stat")).map_or(true, |stat| {
            !stat
                .rsplit(')')
                .next()
                .unwrap_or("")
                .trim_start()
                .starts_with('Z')
        })
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
            |line| {
                grandchild = line.trim().parse::<libc::pid_t>().ok();
                stopper.cancel();
            },
        )
        .await
        .unwrap();
        assert_eq!(ended.stopped, Some(Stop::Pause));
        assert!(ended.status.is_none(), "killed, not exited: {ended:?}");
        assert!(!ended.success());
        assert!(started.elapsed() >= Duration::from_millis(300));
        let grandchild = grandchild.unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while alive(grandchild) && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(!alive(grandchild), "the grandchild survived");
    }

    #[tokio::test]
    async fn a_run_reports_its_lines_status_and_stderr() {
        let mut command = Command::new("sh");
        command
            .args(["-c", "echo one; echo two; echo 'ERROR: bad' >&2; exit 3"])
            .stdin(std::process::Stdio::null());
        let never = CancellationToken::new();
        let mut lines = Vec::new();
        let ended = run_process(command, &never, &never, Duration::from_secs(1), |l| {
            lines.push(l.to_owned());
        })
        .await
        .unwrap();
        assert_eq!(lines, ["one", "two"]);
        assert_eq!(ended.status.and_then(|s| s.code()), Some(3));
        assert_eq!(ended.stopped, None);
        assert_eq!(ended.stderr.trim(), "ERROR: bad");
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
        let ended = run_process(command, &never, &never, Duration::from_secs(1), |l| {
            lines.push(l.to_owned());
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
        let ended = run_process(command, &never, &never, Duration::from_secs(1), |l| {
            lines.push(l.to_owned());
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
