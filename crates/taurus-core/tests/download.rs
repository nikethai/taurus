use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use taurus_core::errors::ErrorKind;
use taurus_core::http::{JobContext, JobOutcome, build_client, probe, resolve, run_job};
use taurus_core::store::{JobId, JobState, NewJob, Store};
use taurus_testkit::{FailAfter, FileSpec, TestServer, content, sha256_hex};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

const MIB: usize = 1024 * 1024;
const PATH: &str = "/x.bin";

struct Fixture {
    _tmp: tempfile::TempDir,
    store: Store,
    dest: PathBuf,
    server: TestServer,
}

impl Fixture {
    async fn new() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(&tmp.path().join("taurus.db")).unwrap();
        let dest = tmp.path().join("out");
        fs::create_dir(&dest).unwrap();
        Fixture {
            _tmp: tmp,
            store,
            dest,
            server: TestServer::start().await,
        }
    }

    /// Serves `spec` and inserts a job for it carrying the spec's probe fields.
    fn add(&self, spec: &FileSpec, connections: u32) -> JobId {
        let url = self.server.serve(PATH, spec.clone());
        let id = self
            .store
            .insert_job(&NewJob {
                url,
                dest_dir: self.dest.clone(),
                file_name: "x.bin".into(),
                connections,
                mirrors: Vec::new(),
                checksum: None,
                referer: None,
                queue_pos: 0,
            })
            .unwrap();
        let mut job = self.store.get_job(id).unwrap().unwrap();
        job.total = Some(spec.size as u64);
        job.resumable = spec.ranges;
        job.etag = spec.etag.clone();
        self.store.update_job(&job).unwrap();
        id
    }

    async fn run(&self, id: JobId, cancel: CancellationToken) -> JobOutcome {
        let (progress, _progress_rx) = mpsc::unbounded_channel();
        let ctx = JobContext {
            id,
            client: build_client(),
            store: self.store.clone(),
            cancel,
            progress,
        };
        tokio::time::timeout(Duration::from_secs(30), run_job(ctx))
            .await
            .expect("job did not finish in 30 s")
    }

    fn tapa(&self) -> PathBuf {
        self.dest.join("x.bin.tapa")
    }
}

fn assert_completed(outcome: JobOutcome, spec: &FileSpec) -> PathBuf {
    match outcome {
        JobOutcome::Completed(path) => {
            let got = fs::read(&path).unwrap();
            assert_eq!(got.len(), spec.size);
            assert_eq!(sha256_hex(&got), sha256_hex(&content(spec)));
            path
        }
        other => panic!("expected Completed, got {other:?}"),
    }
}

#[tokio::test]
async fn probe_reads_size_ranges_name_validators() {
    let fx = Fixture::new().await;
    let mut spec = FileSpec::new(5_000_000);
    spec.etag = Some("\"v1\"".into());
    spec.last_modified = Some("Wed, 21 Oct 2015 07:28:00 GMT".into());
    spec.content_disposition = Some("attachment; filename=\"x.bin\"".into());
    let url = fx.server.serve("/dl/file.dat", spec);

    let p = probe(&build_client(), &url).await.unwrap();
    assert_eq!(p.size, Some(5_000_000));
    assert!(p.ranges);
    assert_eq!(p.file_name, "x.bin");
    assert_eq!(p.etag.as_deref(), Some("\"v1\""));
    assert_eq!(
        p.last_modified.as_deref(),
        Some("Wed, 21 Oct 2015 07:28:00 GMT")
    );
    assert_eq!(p.content_type.as_deref(), Some("application/octet-stream"));
    assert_eq!(p.final_url.as_str(), url);
}

#[tokio::test]
async fn probe_without_ranges() {
    let fx = Fixture::new().await;
    let mut spec = FileSpec::new(123_456);
    spec.ranges = false;
    let url = fx.server.serve("/files/report.pdf", spec);

    let p = probe(&build_client(), &url).await.unwrap();
    assert!(!p.ranges);
    assert_eq!(p.size, Some(123_456));
    assert_eq!(p.file_name, "report.pdf");
}

#[tokio::test]
async fn probe_zero_byte_file_and_status_errors() {
    let fx = Fixture::new().await;
    let url = fx.server.serve("/empty.txt", FileSpec::new(0));
    let p = probe(&build_client(), &url).await.unwrap();
    assert_eq!((p.size, p.ranges), (Some(0), false));

    let mut gone = FileSpec::new(10);
    gone.fail_first = vec![404];
    let url = fx.server.serve("/gone", gone);
    let err = probe(&build_client(), &url).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::LinkExpired);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn segmented_download_matches_hash() {
    let fx = Fixture::new().await;
    let spec = FileSpec::new(20 * MIB);
    let id = fx.add(&spec, 16);

    let path = assert_completed(fx.run(id, CancellationToken::new()).await, &spec);
    assert_eq!(path, fx.dest.join("x.bin"));
    assert!(fx.server.requests(PATH) > 2);
    assert!(!fx.tapa().exists());
    let job = fx.store.get_job(id).unwrap().unwrap();
    assert_eq!(job.state, JobState::Completed);
    assert_eq!(job.error, None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn single_stream_when_no_ranges() {
    let fx = Fixture::new().await;
    let mut spec = FileSpec::new(3 * MIB);
    spec.ranges = false;
    let id = fx.add(&spec, 16);

    assert_completed(fx.run(id, CancellationToken::new()).await, &spec);
    assert_eq!(fx.server.requests(PATH), 1);
    assert_eq!(fx.store.load_segments(id).unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn zero_byte_file() {
    let fx = Fixture::new().await;
    let mut spec = FileSpec::new(0);
    spec.ranges = false;
    let id = fx.add(&spec, 16);

    let path = assert_completed(fx.run(id, CancellationToken::new()).await, &spec);
    assert_eq!(fs::metadata(&path).unwrap().len(), 0);
    assert_eq!(fx.server.requests(PATH), 0);
    assert!(!fx.tapa().exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tiny_file_single_segment() {
    let fx = Fixture::new().await;
    let spec = FileSpec::new(10 * 1024);
    let id = fx.add(&spec, 16);

    assert_completed(fx.run(id, CancellationToken::new()).await, &spec);
    let segments = fx.store.load_segments(id).unwrap();
    assert_eq!(segments.len(), 1);
    assert_eq!(segments[0].done, 10 * 1024);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dropped_connection_retries_segment() {
    let fx = Fixture::new().await;
    let mut spec = FileSpec::new(8 * MIB);
    spec.drop_once_after = Some(3_000_000);
    let id = fx.add(&spec, 16);

    assert_completed(fx.run(id, CancellationToken::new()).await, &spec);
    // 8 segments plus one retry from the durable offset: restarting the
    // segment would re-serve ~0.9 MB of it, restarting the job everything.
    assert!(fx.server.requests(PATH) < 16);
    assert!(fx.server.bytes_served(PATH) < spec.size as u64 + 512 * 1024);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn idle_connection_steals_work() {
    let fx = Fixture::new().await;
    let mut spec = FileSpec::new(8 * MIB);
    // Segment 0 backs off after 1 MiB while segment 1 finishes and steals from it.
    spec.drop_once_after = Some(MIB);
    let id = fx.add(&spec, 2);

    assert_completed(fx.run(id, CancellationToken::new()).await, &spec);
    let mut segments = fx.store.load_segments(id).unwrap();
    assert!(segments.len() > 2, "no steal: {segments:?}");
    // Stolen segments are appended; by offset they tile the file exactly.
    segments.sort_by_key(|s| s.start);
    assert_eq!(segments[0].start, 0);
    assert_eq!(segments.last().unwrap().end, spec.size as u64);
    assert!(
        segments.windows(2).all(|w| w[0].end == w[1].start),
        "{segments:?}"
    );
    assert!(
        segments.iter().all(|s| s.start + s.done == s.end),
        "{segments:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ignored_range_falls_back_to_single_stream() {
    let fx = Fixture::new().await;
    let mut spec = FileSpec::new(4 * MIB);
    spec.ignore_range_after_probe = true;
    let id = fx.add(&spec, 16);

    assert_completed(fx.run(id, CancellationToken::new()).await, &spec);
    let job = fx.store.get_job(id).unwrap().unwrap();
    assert!(!job.resumable);
    let segments = fx.store.load_segments(id).unwrap();
    assert_eq!(segments.len(), 1);
    assert_eq!(segments[0].done, spec.size as u64);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transient_503_then_success() {
    let fx = Fixture::new().await;
    let mut spec = FileSpec::new(3 * MIB);
    spec.fail_first = vec![503, 503];
    let id = fx.add(&spec, 16);

    assert_completed(fx.run(id, CancellationToken::new()).await, &spec);
    assert!(fx.server.requests(PATH) >= 5);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn permanent_404_fails() {
    let fx = Fixture::new().await;
    let mut spec = FileSpec::new(2 * MIB);
    spec.fail_first = vec![404; 20];
    let id = fx.add(&spec, 16);

    match fx.run(id, CancellationToken::new()).await {
        JobOutcome::Failed(e) => assert_eq!(e.kind, ErrorKind::LinkExpired),
        other => panic!("expected Failed, got {other:?}"),
    }
    assert!(!fx.dest.join("x.bin").exists());
    assert!(fx.tapa().exists());
    let job = fx.store.get_job(id).unwrap().unwrap();
    assert_eq!(job.state, JobState::Failed);
    assert_eq!(
        job.error.map(|(kind, _)| kind),
        Some(ErrorKind::LinkExpired)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancel_pauses_and_persists_progress() {
    let fx = Fixture::new().await;
    let mut spec = FileSpec::new(20 * MIB);
    spec.bytes_per_sec = Some(4 * MIB as u64);
    let id = fx.add(&spec, 16);

    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(1500)).await;
        trigger.cancel();
    });
    let outcome = fx.run(id, cancel).await;
    assert!(matches!(outcome, JobOutcome::Paused), "got {outcome:?}");

    let done: u64 = fx
        .store
        .load_segments(id)
        .unwrap()
        .iter()
        .map(|s| s.done)
        .sum();
    let on_disk = fs::metadata(fx.tapa()).unwrap().len();
    assert!(done > 0);
    assert!(done <= on_disk, "recorded {done} > {on_disk} bytes on disk");
    assert!(done < spec.size as u64);
    assert_eq!(
        fx.store.get_job(id).unwrap().unwrap().state,
        JobState::Paused
    );

    // Running the job again resumes from the recorded progress.
    let mut fast = spec.clone();
    fast.bytes_per_sec = None;
    fx.server.replace(PATH, fast);
    let served_before = fx.server.bytes_served(PATH);
    assert_completed(fx.run(id, CancellationToken::new()).await, &spec);
    // Not a restart: well under a full re-download (steals may over-read a little).
    assert!(fx.server.bytes_served(PATH) - served_before < spec.size as u64);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn size_change_on_server_fails_without_stitching() {
    let fx = Fixture::new().await;
    let spec = FileSpec::new(3 * MIB);
    let id = fx.add(&spec, 16);
    // The job was probed when the file was one byte longer.
    let mut job = fx.store.get_job(id).unwrap().unwrap();
    job.total = Some(spec.size as u64 + 1);
    fx.store.update_job(&job).unwrap();

    match fx.run(id, CancellationToken::new()).await {
        JobOutcome::Failed(e) => assert_eq!(e.kind, ErrorKind::FileChangedOnServer),
        other => panic!("expected Failed, got {other:?}"),
    }
    assert!(!fx.dest.join("x.bin").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stale_open_ended_segment_is_replanned() {
    let fx = Fixture::new().await;
    let spec = FileSpec::new(3 * MIB);
    let id = fx.add(&spec, 16);
    // Left over from an earlier unranged run; the job is resumable again now.
    fs::write(fx.tapa(), &content(&spec)[..1000]).unwrap();
    let stale = taurus_core::plan::Segment {
        start: 0,
        end: u64::MAX,
        done: 1000,
    };
    fx.store.replace_segments(id, &[stale]).unwrap();

    assert_completed(fx.run(id, CancellationToken::new()).await, &spec);
    assert_eq!(fx.store.load_segments(id).unwrap().len(), 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gapped_segment_list_is_replanned() {
    let fx = Fixture::new().await;
    let spec = FileSpec::new(4 * MIB);
    let id = fx.add(&spec, 16);
    // A hole at [1 MiB, 3 MiB): never trust such a list, even with progress.
    fs::write(fx.tapa(), &content(&spec)[..MIB]).unwrap();
    let mib = MIB as u64;
    let gapped = [
        taurus_core::plan::Segment {
            start: 0,
            end: mib,
            done: mib,
        },
        taurus_core::plan::Segment {
            start: 3 * mib,
            end: 4 * mib,
            done: 0,
        },
    ];
    fx.store.replace_segments(id, &gapped).unwrap();

    assert_completed(fx.run(id, CancellationToken::new()).await, &spec);
    let mut segments = fx.store.load_segments(id).unwrap();
    segments.sort_by_key(|s| s.start);
    assert_eq!(segments[0].start, 0);
    assert_eq!(segments.last().unwrap().end, spec.size as u64);
    assert!(segments.windows(2).all(|w| w[0].end == w[1].start));
}

/// A "can't scan for viruses" page whose form submits `/file?confirm=t`.
const CONFIRM_PAGE: &str = r#"<html><body><p>This file is too large to scan for viruses.</p>
<form id="download-form" action="/file" method="get">
<input type="submit" value="Download anyway">
<input type="hidden" name="confirm" value="t">
</form></body></html>"#;

const QUOTA_PAGE: &str = "<html><body><p>Sorry, you can't view or download this file at \
    this time.</p><p>Too many users have viewed or downloaded this file recently. Please \
    try accessing the file again later.</p></body></html>";

async fn resolve_err(url: &str) -> taurus_core::CoreError {
    resolve(&build_client(), url, None).await.unwrap_err()
}

fn html_after(after_requests: usize, status: u16, headers: &[(&str, &str)]) -> FailAfter {
    FailAfter {
        after_requests,
        status,
        headers: headers
            .iter()
            .map(|&(n, v)| (n.to_owned(), v.to_owned()))
            .collect(),
    }
}

#[tokio::test]
async fn resolve_follows_one_confirm_form() {
    let fx = Fixture::new().await;
    let page = fx.server.serve_page("/drive", 200, &[], CONFIRM_PAGE);
    let file = fx.server.serve("/file", FileSpec::new(2 * MIB));

    let p = resolve(&build_client(), &page, None).await.unwrap();
    assert_eq!(p.size, Some(2 * MIB as u64));
    assert!(p.ranges);
    assert_eq!(p.requested_url.as_str(), format!("{file}?confirm=t"));
    assert_eq!(fx.server.requests("/drive"), 1);
    assert_eq!(fx.server.requests("/file"), 1);
}

#[tokio::test]
async fn resolve_follows_only_one_confirm_form() {
    let fx = Fixture::new().await;
    let page = fx.server.serve_page("/drive", 200, &[], CONFIRM_PAGE);
    fx.server.serve_page("/file", 200, &[], CONFIRM_PAGE);

    let err = resolve_err(&page).await;
    assert_eq!(err.kind, ErrorKind::SiteNotSupported);
    assert!(err.detail.contains("repeated"), "{}", err.detail);
    assert_eq!(fx.server.requests("/file"), 1);
}

#[tokio::test]
async fn resolve_sends_referer_on_every_request() {
    let fx = Fixture::new().await;
    let page = fx.server.serve_page("/drive", 200, &[], CONFIRM_PAGE);
    fx.server.serve("/file", FileSpec::new(1000));

    resolve(&build_client(), &page, Some("https://ref.example/post"))
        .await
        .unwrap();
    for path in ["/drive", "/file"] {
        let headers = fx.server.last_request_headers(path);
        assert!(
            headers.contains(&("referer".into(), "https://ref.example/post".into())),
            "{path}: {headers:?}"
        );
    }
}

#[tokio::test]
async fn resolve_bot_check_fails() {
    let fx = Fixture::new().await;
    let challenge = [("cf-mitigated", "challenge")];
    let url = fx
        .server
        .serve_page("/dl/a.iso", 403, &challenge, "<html>checking</html>");
    assert_eq!(resolve_err(&url).await.kind, ErrorKind::BlockedByBotCheck);

    let cloudflare = [("server", "cloudflare")];
    let body = "<html><head><title>Just a moment...</title></head></html>";
    let url = fx.server.serve_page("/dl/b.iso", 403, &cloudflare, body);
    assert_eq!(resolve_err(&url).await.kind, ErrorKind::BlockedByBotCheck);
}

#[tokio::test]
async fn resolve_quota_fails() {
    let fx = Fixture::new().await;
    let url = fx.server.serve_page("/uc", 200, &[], QUOTA_PAGE);
    assert_eq!(resolve_err(&url).await.kind, ErrorKind::QuotaExceeded);
}

#[tokio::test]
async fn resolve_web_page_fails() {
    let fx = Fixture::new().await;
    let url = fx.server.serve_page(
        "/files/",
        200,
        &[],
        "<html><body>Index of /files</body></html>",
    );
    let err = resolve_err(&url).await;
    assert_eq!(err.kind, ErrorKind::SiteNotSupported);
    assert_eq!(err.detail, "this link is a web page");
}

#[tokio::test]
async fn resolve_captive_portal_fails() {
    let fx = Fixture::new().await;
    let url = fx.server.serve_page(
        "/files/a.iso",
        200,
        &[],
        "<html><body>Hotel Wi-Fi login</body></html>",
    );
    assert_eq!(resolve_err(&url).await.kind, ErrorKind::CaptivePortal);
}

#[tokio::test]
async fn resolve_keeps_status_errors_for_error_pages() {
    let fx = Fixture::new().await;
    let url = fx
        .server
        .serve_page("/gone.iso", 404, &[], "<html><body>Not Found</body></html>");
    assert_eq!(resolve_err(&url).await.kind, ErrorKind::LinkExpired);
}

#[tokio::test]
async fn resolve_rereads_ranged_html_without_range() {
    let fx = Fixture::new().await;
    // An HTML "file" that honors bytes=0-0: one byte is not enough to judge.
    let mut spec = FileSpec::new(4000);
    spec.content_type = Some("text/html".into());
    let url = fx.server.serve("/page", spec);

    assert_eq!(resolve_err(&url).await.kind, ErrorKind::SiteNotSupported);
    assert_eq!(fx.server.requests("/page"), 2);
    let last = fx.server.last_request_headers("/page");
    assert!(last.iter().all(|(name, _)| name != "range"), "{last:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn html_mid_download_is_not_written() {
    let fx = Fixture::new().await;
    let mut spec = FileSpec::new(8 * MIB);
    // Requests after the third get a 200 web page, like expired hotel Wi-Fi.
    spec.fail_after = Some(html_after(3, 200, &[("content-type", "text/html")]));
    let id = fx.add(&spec, 16);

    match fx.run(id, CancellationToken::new()).await {
        JobOutcome::Failed(e) => assert_eq!(e.kind, ErrorKind::CaptivePortal),
        other => panic!("expected Failed, got {other:?}"),
    }
    let on_disk = fs::read(fx.tapa()).unwrap();
    let expected = content(&spec);
    for s in fx
        .store
        .load_segments(id)
        .unwrap()
        .iter()
        .filter(|s| s.done > 0)
    {
        let (start, end) = (s.start as usize, (s.start + s.done) as usize);
        assert!(
            end <= on_disk.len(),
            "recorded {end} > {} on disk",
            on_disk.len()
        );
        assert!(
            on_disk[start..end] == expected[start..end],
            "bytes {start}..{end} differ"
        );
    }
    assert!(!on_disk.windows(5).any(|w| w == b"<html"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bot_check_mid_download_fails() {
    let fx = Fixture::new().await;
    let mut spec = FileSpec::new(8 * MIB);
    spec.fail_after = Some(html_after(3, 403, &[("cf-mitigated", "challenge")]));
    let id = fx.add(&spec, 16);

    match fx.run(id, CancellationToken::new()).await {
        JobOutcome::Failed(e) => assert_eq!(e.kind, ErrorKind::BlockedByBotCheck),
        other => panic!("expected Failed, got {other:?}"),
    }
}
