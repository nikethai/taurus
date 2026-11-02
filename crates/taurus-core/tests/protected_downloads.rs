//! Passwords for protected (HTTP Basic auth) downloads: sent only where they
//! belong, kept only in the secret store, never written or logged.

use std::fmt::Debug;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use base64::Engine as _;
use taurus_core::CoreError;
use taurus_core::secrets::{MemoryStore, SecretStore};
use taurus_core::store::Store;
use taurus_core::{
    AddRequest, Credentials, Engine, EngineConfig, ErrorKind, JobId, JobState, NetworkSettings,
    ProxySetting, RemoveFiles, Settings,
};
use taurus_testkit::{FileSpec, TestServer, content, sha256_hex};
use tokio::sync::broadcast;

const MIB: usize = 1024 * 1024;
const USER: &str = "hyades-user-7";
const PASSWORD: &str = "s3cr3t-Pa55";
const TIMEOUT: Duration = Duration::from_secs(30);

struct Fixture {
    tmp: tempfile::TempDir,
    dest: PathBuf,
    server: TestServer,
    secrets: Arc<MemoryStore>,
}

impl Fixture {
    async fn new() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("out");
        fs::create_dir(&dest).unwrap();
        Fixture {
            tmp,
            dest,
            server: TestServer::start().await,
            secrets: Arc::new(MemoryStore::new()),
        }
    }

    fn data_dir(&self) -> PathBuf {
        self.tmp.path().join("data")
    }

    /// Never the real keychain: the fixture's memory store.
    fn config(&self) -> EngineConfig {
        let mut config = EngineConfig::new(self.data_dir());
        config.secrets = self.secrets.clone();
        config
    }

    async fn engine(&self) -> Engine {
        Engine::open(self.config()).await.unwrap()
    }

    /// The keychain account of the job: `job:<id>:<database token>`.
    fn account(&self, id: JobId) -> String {
        let store = Store::open(&self.data_dir().join("taurus.db")).unwrap();
        format!("job:{}:{}", id.0, store.database_token().unwrap())
    }

    fn request(&self, url: &str, credentials: Option<Credentials>) -> AddRequest {
        AddRequest {
            url: url.to_string(),
            dest_dir: self.dest.clone(),
            credentials,
            ..AddRequest::default()
        }
    }
}

fn creds(password: &str) -> Option<Credentials> {
    Some(Credentials {
        username: USER.into(),
        password: password.into(),
    })
}

fn protected(size: usize) -> FileSpec {
    let mut spec = FileSpec::new(size);
    spec.basic_auth = Some((USER.into(), PASSWORD.into()));
    spec
}

async fn wait(engine: &Engine, id: JobId) -> JobState {
    tokio::time::timeout(TIMEOUT, engine.wait(id))
        .await
        .expect("job did not settle in 30 s")
}

fn error_of(engine: &Engine, id: JobId) -> (ErrorKind, String) {
    let view = engine.snapshot().into_iter().find(|v| v.id == id).unwrap();
    view.error.expect("the job has an error")
}

fn assert_file(path: &Path, spec: &FileSpec) {
    let got = fs::read(path).unwrap();
    assert_eq!(sha256_hex(&got), sha256_hex(&content(spec)));
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(n, _)| n == name)
        .map(|(_, v)| v.as_str())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn protected_file_without_credentials_needs_sign_in() {
    let fx = Fixture::new().await;
    let engine = fx.engine().await;
    let url = fx.server.serve("/p.bin", protected(1000));
    let err = engine.add(fx.request(&url, None)).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::NeedsSignIn);
    assert!(err.detail.contains("\"test\""), "{}", err.detail);
    assert!(engine.snapshot().is_empty());
    assert!(header(&fx.server.last_request_headers("/p.bin"), "authorization").is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn with_credentials_downloads() {
    let fx = Fixture::new().await;
    let engine = fx.engine().await;
    let mut spec = protected(3 * MIB);
    spec.bytes_per_sec = Some(4 * MIB as u64);
    let url = fx.server.serve("/p.bin", spec.clone());
    let mut req = fx.request(&url, creds(PASSWORD));
    req.connections = Some(4);
    let id = engine.add(req).await.unwrap();
    // Paused and resumed: the resume's check of the link signs in too.
    tokio::time::sleep(Duration::from_millis(300)).await;
    engine.pause(id).await.unwrap();
    assert_eq!(wait(&engine, id).await, JobState::Paused);
    engine.resume(id).await.unwrap();
    assert_eq!(wait(&engine, id).await, JobState::Completed);
    assert_file(&fx.dest.join("p.bin"), &spec);
    // Every request was let in: the probe, the resume's and the segments'.
    assert!(fx.server.requests("/p.bin") >= 4);
    let headers = fx.server.last_request_headers("/p.bin");
    assert!(header(&headers, "authorization").is_some_and(|v| v.starts_with("Basic ")));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wrong_password_reports_clearly() {
    let fx = Fixture::new().await;
    let engine = fx.engine().await;
    let url = fx.server.serve("/p.bin", protected(1000));
    let err = engine
        .add(fx.request(&url, creds("not-it")))
        .await
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::NeedsSignIn);
    assert_eq!(err.detail, "wrong user name or password");
    assert!(engine.snapshot().is_empty());
    assert_eq!(fx.secrets.get(&fx.account(JobId(1))).unwrap(), None);
}

/// No keychain: the password lived in memory, so after a restart the job
/// asks for it again; giving it resumes the job.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn set_credentials_resumes_failed_job() {
    let mut fx = Fixture::new().await;
    let spec = protected(2 * MIB);
    let url = fx.server.serve("/p.bin", spec.clone());
    let mut config = fx.config();
    config.autostart = false;
    let engine = Engine::open(config).await.unwrap();
    let id = engine.add(fx.request(&url, creds(PASSWORD))).await.unwrap();
    engine.shutdown().await.unwrap();

    // The next session has a store without the password.
    fx.secrets = Arc::new(MemoryStore::new());
    let engine = fx.engine().await;
    assert_eq!(wait(&engine, id).await, JobState::Failed);
    let (kind, detail) = error_of(&engine, id);
    assert_eq!(kind, ErrorKind::NeedsSignIn);
    assert!(detail.contains("\"test\""), "{detail}");

    // A wrong one fails again, clearly; the right one finishes the job.
    let err = engine
        .set_credentials(id, creds("not-it").unwrap())
        .await
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::NeedsSignIn);
    assert_eq!(err.detail, "wrong user name or password");
    // The job says so too, and is still Failed.
    assert_eq!(engine.wait(id).await, JobState::Failed);
    assert_eq!(error_of(&engine, id).1, "wrong user name or password");
    engine
        .set_credentials(id, creds(PASSWORD).unwrap())
        .await
        .unwrap();
    assert_eq!(wait(&engine, id).await, JobState::Completed);
    assert_file(&fx.dest.join("p.bin"), &spec);
    assert_eq!(
        fx.secrets.get(&fx.account(id)).unwrap().as_deref(),
        Some(PASSWORD)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn remove_deletes_secret() {
    let fx = Fixture::new().await;
    let mut config = fx.config();
    config.autostart = false;
    let engine = Engine::open(config).await.unwrap();
    for (i, files) in [
        RemoveFiles::Keep,
        RemoveFiles::All,
        RemoveFiles::IfUnfinished,
    ]
    .into_iter()
    .enumerate()
    {
        let url = fx.server.serve(&format!("/p{i}.bin"), protected(1000));
        let id = engine.add(fx.request(&url, creds(PASSWORD))).await.unwrap();
        let account = fx.account(id);
        assert_eq!(fx.secrets.get(&account).unwrap().as_deref(), Some(PASSWORD));
        engine.remove_with(id, files).await.unwrap();
        assert_eq!(fx.secrets.get(&account).unwrap(), None, "{files:?}");
    }
}

/// Credentials go to the link's own host only: not to a host a redirect
/// leads to, nor to a mirror elsewhere.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn credentials_stay_on_their_host() {
    let fx = Fixture::new().await;
    let other = TestServer::start().await;
    let engine = fx.engine().await;
    let spec = FileSpec::new(MIB);
    let elsewhere = other.serve("/f.bin", spec.clone());
    let link = fx.server.redirect_cycle("/r.bin", &[&elsewhere]);
    let id = engine
        .add(fx.request(&link, creds(PASSWORD)))
        .await
        .unwrap();
    assert_eq!(wait(&engine, id).await, JobState::Completed);
    assert_file(&fx.dest.join("f.bin"), &spec);
    let sent = fx.server.last_request_headers("/r.bin");
    assert!(header(&sent, "authorization").is_some(), "{sent:?}");
    let received = other.last_request_headers("/f.bin");
    assert!(other.requests("/f.bin") > 0);
    assert!(header(&received, "authorization").is_none(), "{received:?}");

    // A mirror on another host serves segments without them.
    let mut big = protected(4 * MIB);
    big.seed = 9;
    let primary = fx.server.serve("/m.bin", big.clone());
    let mut open = big.clone();
    open.basic_auth = None;
    let mirror = other.serve("/m.bin", open);
    let mut req = fx.request(&primary, creds(PASSWORD));
    req.mirrors = vec![mirror];
    req.connections = Some(4);
    let id = engine.add(req).await.unwrap();
    assert_eq!(wait(&engine, id).await, JobState::Completed);
    assert_file(&fx.dest.join("m.bin"), &big);
    assert!(other.requests("/m.bin") > 1, "the mirror served segments");
    let received = other.last_request_headers("/m.bin");
    assert!(header(&received, "authorization").is_none(), "{received:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn password_never_persisted_or_logged() {
    let logs = capture_logs();
    let fx = Fixture::new().await;
    // A proxy password saved by hand (the engine refuses one, R7) is
    // dropped when the settings load, without being logged.
    let proxy_password = "Pr0xy-Secret-9";
    fs::create_dir_all(fx.data_dir()).unwrap();
    Store::open(&fx.data_dir().join("taurus.db"))
        .unwrap()
        .save_settings(&Settings {
            network: NetworkSettings {
                proxy: ProxySetting::Manual {
                    url: format!("http://proxy-user:{proxy_password}@127.0.0.1:9"),
                },
            },
            ..Settings::default()
        })
        .unwrap();
    let engine = fx.engine().await;
    assert_eq!(engine.settings().network.proxy, ProxySetting::System);
    let mut events = engine.subscribe();
    let spec = protected(2 * MIB);
    let url = fx.server.serve("/p.bin", spec.clone());
    // The settings are saved during the run too.
    let mut settings = engine.settings();
    settings.auto_checksum = !settings.auto_checksum;
    engine.set_settings(settings).await.unwrap();
    // A wrong password first: its failure is reported and logged without it.
    let wrong = "wrong-Pa55-xyz";
    let err = engine
        .add(fx.request(&url, creds(wrong)))
        .await
        .unwrap_err();
    assert!(!format!("{err:?} {err}").contains(wrong));
    let id = engine.add(fx.request(&url, creds(PASSWORD))).await.unwrap();
    assert_eq!(wait(&engine, id).await, JobState::Completed);
    assert_file(&fx.dest.join("p.bin"), &spec);

    let secrets = [
        PASSWORD.to_owned(),
        wrong.to_owned(),
        base64::engine::general_purpose::STANDARD.encode(format!("{USER}:{PASSWORD}")),
        base64::engine::general_purpose::STANDARD.encode(format!("{USER}:{wrong}")),
    ];
    // While the engine runs, the WAL holds the latest writes.
    let data = fx.data_dir();
    assert!(data.join("taurus.db").exists() && data.join("taurus.db-wal").exists());
    let mut scanned = scan_tree(&data, &secrets, read_shared);
    scan_tree(&fx.dest, &secrets, read_shared);
    engine.shutdown().await.unwrap();
    // The last handle closes the database: SQLite then checkpoints the WAL
    // into it and deletes the WAL and its index, so nothing locks the files
    // and the database holds everything that was ever in the WAL's
    // current pages.
    drop(engine);
    let closed = tokio::time::timeout(TIMEOUT, async {
        while data.join("taurus.db-wal").exists() || data.join("taurus.db-shm").exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    assert!(closed.is_ok(), "the database was not closed");
    scanned.extend(scan_tree(&data, &secrets, |path| fs::read(path).unwrap()));
    // The scan reads what is stored: the user name is there.
    assert!(
        scanned.iter().any(|bytes| find(bytes, USER.as_bytes())),
        "the user name is stored"
    );

    let mut seen = 0;
    loop {
        match events.try_recv() {
            Ok(event) => {
                let json = serde_json::to_string(&event).unwrap();
                let debug = format!("{event:?}");
                // Never the event itself in the message: it would hold the
                // secret.
                for (i, secret) in secrets
                    .iter()
                    .map(String::as_str)
                    .chain([proxy_password])
                    .enumerate()
                {
                    assert!(
                        !json.contains(secret) && !debug.contains(secret),
                        "secret #{i} appeared in event #{seen}"
                    );
                }
                seen += 1;
            }
            Err(broadcast::error::TryRecvError::Lagged(n)) => panic!("{n} events lost"),
            Err(_) => break,
        }
    }
    assert!(seen > 3, "events were captured ({seen})");

    let logs = logs.lock().unwrap().clone();
    assert!(
        logs.contains(&format!("job={}", id.0)),
        "logs were captured"
    );
    assert!(
        logs.contains("ignoring the saved proxy"),
        "the proxy was dropped"
    );
    for (i, secret) in secrets
        .iter()
        .map(String::as_str)
        .chain([proxy_password])
        .enumerate()
    {
        assert!(
            !logs.contains(secret),
            "secret #{i} appeared in the log ({} bytes)",
            logs.len()
        );
    }
    #[cfg(target_os = "linux")]
    for name in xattr::list(fx.dest.join("p.bin")).unwrap() {
        if let Some(value) = xattr::get(fx.dest.join("p.bin"), &name).unwrap() {
            for secret in &secrets {
                assert!(!find(&value, secret.as_bytes()), "{name:?}");
            }
        }
    }
}

/// Job ids repeat across data dirs, the keychain is one: each data dir's
/// job 1 keeps its own password.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn data_dirs_never_share_a_password() {
    let a = Fixture::new().await;
    let mut b = Fixture::new().await;
    b.secrets = a.secrets.clone();
    let (pa, pb) = ("pass-for-a-1", "pass-for-b-2");
    let mut spec_a = FileSpec::new(MIB);
    spec_a.basic_auth = Some((USER.into(), pa.into()));
    let mut spec_b = FileSpec::new(MIB);
    spec_b.seed = 5;
    spec_b.basic_auth = Some((USER.into(), pb.into()));
    let url_a = a.server.serve("/p.bin", spec_a.clone());
    let url_b = b.server.serve("/p.bin", spec_b.clone());
    let (ea, eb) = (a.engine().await, b.engine().await);
    let ia = ea.add(a.request(&url_a, creds(pa))).await.unwrap();
    let ib = eb.add(b.request(&url_b, creds(pb))).await.unwrap();
    assert_eq!((ia, ib), (JobId(1), JobId(1)));
    assert_ne!(a.account(ia), b.account(ib));
    assert_eq!(wait(&ea, ia).await, JobState::Completed);
    assert_eq!(wait(&eb, ib).await, JobState::Completed);
    assert_file(&a.dest.join("p.bin"), &spec_a);
    assert_file(&b.dest.join("p.bin"), &spec_b);
    assert_eq!(a.secrets.get(&a.account(ia)).unwrap().as_deref(), Some(pa));
    assert_eq!(a.secrets.get(&b.account(ib)).unwrap().as_deref(), Some(pb));
    // Removing one leaves the other.
    eb.remove(ib, false).await.unwrap();
    assert_eq!(a.secrets.get(&b.account(ib)).unwrap(), None);
    assert_eq!(a.secrets.get(&a.account(ia)).unwrap().as_deref(), Some(pa));
}

/// `set_credentials` resumes only a job that failed to sign in.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn set_credentials_leaves_other_jobs_alone() {
    let fx = Fixture::new().await;
    let mut config = fx.config();
    config.autostart = false;
    let engine = Engine::open(config).await.unwrap();
    // Paused.
    let url = fx.server.serve("/p.bin", protected(1000));
    let paused = engine.add(fx.request(&url, creds(PASSWORD))).await.unwrap();
    engine.pause(paused).await.unwrap();
    let before = fx.server.requests("/p.bin");
    engine
        .set_credentials(paused, creds(PASSWORD).unwrap())
        .await
        .unwrap();
    assert_eq!(wait(&engine, paused).await, JobState::Paused);
    assert_eq!(fx.server.requests("/p.bin"), before);

    // Failed for another reason: the file is gone.
    let gone = fx.server.serve("/gone.bin", FileSpec::new(1000));
    let failed = engine.add(fx.request(&gone, None)).await.unwrap();
    let mut spec = FileSpec::new(1000);
    spec.fail_first = vec![410; 50];
    fx.server.replace("/gone.bin", spec);
    engine.start_now(failed).await.unwrap();
    assert_eq!(wait(&engine, failed).await, JobState::Failed);
    let error = error_of(&engine, failed);
    assert_ne!(error.0, ErrorKind::NeedsSignIn);
    let before = fx.server.requests("/gone.bin");
    engine
        .set_credentials(failed, creds(PASSWORD).unwrap())
        .await
        .unwrap();
    assert_eq!(wait(&engine, failed).await, JobState::Failed);
    assert_eq!(error_of(&engine, failed), error);
    assert_eq!(fx.server.requests("/gone.bin"), before);
}

/// A keychain that is not there.
struct NoKeychain;

fn absent() -> CoreError {
    CoreError {
        kind: ErrorKind::Unknown,
        detail: "no keychain here".into(),
    }
}

impl SecretStore for NoKeychain {
    fn set(&self, _: &str, _: &str) -> Result<(), CoreError> {
        Err(absent())
    }
    fn get(&self, _: &str) -> Result<Option<String>, CoreError> {
        Err(absent())
    }
    fn delete(&self, _: &str) -> Result<(), CoreError> {
        Err(absent())
    }
}

/// Without a keychain the password lives in memory: the job downloads, a
/// warning (without the password) says so, and the next session asks again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn without_a_keychain_the_password_lives_for_the_session() {
    let logs = capture_logs();
    let fx = Fixture::new().await;
    let mut config = fx.config();
    config.secrets = Arc::new(NoKeychain);
    let engine = Engine::open(config).await.unwrap();
    let spec = protected(MIB);
    let url = fx.server.serve("/p.bin", spec.clone());
    let id = engine.add(fx.request(&url, creds(PASSWORD))).await.unwrap();
    assert_eq!(wait(&engine, id).await, JobState::Completed);
    assert_file(&fx.dest.join("p.bin"), &spec);
    let logged = logs.lock().unwrap().clone();
    assert!(
        logged.contains("keeping the password in memory until the engine stops"),
        "the fallback is logged"
    );
    assert!(!logged.contains(PASSWORD));
    engine.shutdown().await.unwrap();

    // The next session: the job runs again without a password.
    let path = "/q.bin";
    let url = fx.server.serve(path, protected(1000));
    let mut config = fx.config();
    config.secrets = Arc::new(NoKeychain);
    config.autostart = false;
    let engine = Engine::open(config).await.unwrap();
    let id = engine.add(fx.request(&url, creds(PASSWORD))).await.unwrap();
    engine.shutdown().await.unwrap();
    let mut config = fx.config();
    config.secrets = Arc::new(NoKeychain);
    let engine = Engine::open(config).await.unwrap();
    assert_eq!(wait(&engine, id).await, JobState::Failed);
    assert_eq!(error_of(&engine, id).0, ErrorKind::NeedsSignIn);
}

/// A mirror on the link's own origin signs in too.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_mirror_on_the_same_host_signs_in() {
    let fx = Fixture::new().await;
    let engine = fx.engine().await;
    let spec = protected(4 * MIB);
    let primary = fx.server.serve("/p.bin", spec.clone());
    let mirror = fx.server.serve("/mirror/p.bin", spec.clone());
    let mut req = fx.request(&primary, creds(PASSWORD));
    req.mirrors = vec![mirror];
    req.connections = Some(4);
    let id = engine.add(req).await.unwrap();
    assert_eq!(wait(&engine, id).await, JobState::Completed);
    assert_file(&fx.dest.join("p.bin"), &spec);
    // The mirror's check and at least one segment.
    assert!(fx.server.requests("/mirror/p.bin") > 1);
    let sent = fx.server.last_request_headers("/mirror/p.bin");
    assert!(header(&sent, "authorization").is_some());
}

/// Redirects: within the host the credentials stay; a hop through another
/// host drops them there, and they are sent again only back on the link's
/// host; a protected other host is asked without them.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn redirects_keep_credentials_on_their_host_only() {
    let fx = Fixture::new().await;
    let other = TestServer::start().await;
    let engine = fx.engine().await;

    // Same host, same scheme.
    let spec = protected(MIB);
    let target = fx.server.serve("/same/p.bin", spec.clone());
    let link = fx.server.redirect_cycle("/same/r.bin", &[&target]);
    let id = engine
        .add(fx.request(&link, creds(PASSWORD)))
        .await
        .unwrap();
    assert_eq!(wait(&engine, id).await, JobState::Completed);
    assert_file(&fx.dest.join("p.bin"), &spec);

    // A -> B -> A: B never gets them, A gets them back.
    let mut spec = protected(MIB);
    spec.seed = 3;
    let back = fx.server.serve("/back/q.bin", spec.clone());
    let hop = other.redirect_cycle("/hop", &[&back]);
    let link = fx.server.redirect_cycle("/back/r.bin", &[&hop]);
    let id = engine
        .add(fx.request(&link, creds(PASSWORD)))
        .await
        .unwrap();
    assert_eq!(wait(&engine, id).await, JobState::Completed);
    assert_file(&fx.dest.join("q.bin"), &spec);
    assert!(other.requests("/hop") > 0);
    assert!(header(&other.last_request_headers("/hop"), "authorization").is_none());
    // A wrong password through the same hops is reported as wrong.
    let err = engine
        .add(fx.request(&link, creds("not-it")))
        .await
        .unwrap_err();
    assert_eq!(err.detail, "wrong user name or password");

    // A protected other host is asked without them: it names its realm.
    let elsewhere = other.serve("/locked.bin", protected(1000));
    let link = fx.server.redirect_cycle("/to-locked", &[&elsewhere]);
    let err = engine
        .add(fx.request(&link, creds(PASSWORD)))
        .await
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::NeedsSignIn);
    assert!(err.detail.contains("\"test\""), "{}", err.detail);
    assert!(header(&other.last_request_headers("/locked.bin"), "authorization").is_none());
}

/// Reads every file under `dir` with `read_file` and fails if one holds a
/// secret (naming the file only); returns what it read.
fn scan_tree(dir: &Path, secrets: &[String], read_file: fn(&Path) -> Vec<u8>) -> Vec<Vec<u8>> {
    let mut read = Vec::new();
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            read.extend(scan_tree(&path, secrets, read_file));
            continue;
        }
        let bytes = read_file(&path);
        for secret in secrets {
            assert!(
                !find(&bytes, secret.as_bytes()),
                "{} holds a secret",
                path.display()
            );
        }
        read.push(bytes);
    }
    read
}

/// The file's bytes, readable while SQLite has the database open: on
/// Windows its byte-range locks (on the `-shm` index, or the lock bytes of
/// a database past 1 GiB) fail reads that touch them (ERROR_LOCK_VIOLATION),
/// so a block that cannot be read for that reason alone reads as zeros. The
/// locked bytes are SQLite's lock slots, never stored data.
fn read_shared(path: &Path) -> Vec<u8> {
    use std::io::{Read, Seek, SeekFrom};
    const BLOCK: usize = 512;
    const ERROR_LOCK_VIOLATION: i32 = 33;
    let mut file = fs::File::open(path).unwrap();
    let mut bytes = Vec::new();
    let mut block = [0; BLOCK];
    loop {
        file.seek(SeekFrom::Start(bytes.len() as u64)).unwrap();
        match file.read(&mut block) {
            Ok(0) => return bytes,
            Ok(n) => bytes.extend_from_slice(&block[..n]),
            Err(e) if cfg!(windows) && e.raw_os_error() == Some(ERROR_LOCK_VIOLATION) => {
                bytes.extend_from_slice(&[0; BLOCK]);
            }
            Err(e) => panic!("{}: {e}", path.display()),
        }
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// Everything traced in this test binary, at every level, from the moment
/// this is first called.
fn capture_logs() -> Arc<Mutex<String>> {
    static LOGS: OnceLock<Arc<Mutex<String>>> = OnceLock::new();
    LOGS.get_or_init(|| {
        let logs = Arc::new(Mutex::new(String::new()));
        tracing::subscriber::set_global_default(Capture {
            logs: logs.clone(),
            next_span: AtomicU64::new(1),
        })
        .expect("no other subscriber in this test binary");
        logs
    })
    .clone()
}

struct Capture {
    logs: Arc<Mutex<String>>,
    next_span: AtomicU64,
}

struct Writer<'a>(&'a Mutex<String>);

impl tracing::field::Visit for Writer<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn Debug) {
        let line = format!("{}={value:?} ", field.name());
        self.0.lock().unwrap().push_str(&line);
    }
}

impl tracing::Subscriber for Capture {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        span.record(&mut Writer(&self.logs));
        tracing::span::Id::from_u64(self.next_span.fetch_add(1, Ordering::Relaxed))
    }

    fn record(&self, _: &tracing::span::Id, values: &tracing::span::Record<'_>) {
        values.record(&mut Writer(&self.logs));
    }

    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}

    fn event(&self, event: &tracing::Event<'_>) {
        event.record(&mut Writer(&self.logs));
        self.logs.lock().unwrap().push('\n');
    }

    fn enter(&self, _: &tracing::span::Id) {}

    fn exit(&self, _: &tracing::span::Id) {}
}
