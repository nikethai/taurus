//! One install: download every asset into `.incoming/`, verify it against
//! the publisher's checksum, unpack the listed files, run the version
//! command, then switch `current` and prune old versions.

use std::fs::File;
use std::io::{self, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use reqwest::header::ACCEPT;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use url::Url;

use crate::errors::{CoreError, ErrorKind, classify_io};
use crate::http::client::request_error;

use super::CURRENT_FILE;
use super::catalog::{Asset, ChecksumSource, Layout, parse_sidecar, parse_sums};

const INCOMING: &str = ".incoming";
/// Download progress is reported at most once per this many bytes.
const REPORT_STEP: u64 = 512 * 1024;
/// ffmpeg's version: this many hex digits of its archive's SHA-256.
const HASH_VERSION_LEN: usize = 12;
/// Attempts at running a binary that exec refuses as busy (ETXTBSY).
const BUSY_ATTEMPTS: u32 = 10;

/// `(downloaded, total)` of the install's downloads so far.
pub(super) type Report<'a> = &'a (dyn Fn(u64, Option<u64>) + Send + Sync);

pub(super) struct Install<'a> {
    pub client: &'a reqwest::Client,
    /// `tools/<tool>`.
    pub dir: PathBuf,
    pub assets: Vec<Asset>,
    pub version_arg: &'static str,
    pub version_timeout: Duration,
}

struct Downloaded {
    file: PathBuf,
    sha256: String,
    layout: Layout,
}

impl Install<'_> {
    /// The version the first asset would install, from its published
    /// checksum, without downloading it: for ffmpeg, whose version is its
    /// archive's hash.
    pub async fn expected_version(&self) -> Result<String, CoreError> {
        let first = &self.assets[0];
        let hash = match &first.checksum {
            ChecksumSource::SidecarOfResolved => {
                let resolved = get(self.client, first.url.clone()).await?.url().clone();
                sidecar_hash(self.client, sidecar_of(&resolved)).await?
            }
            source => published_hash(self.client, source).await?,
        };
        Ok(hash_version(&hash))
    }

    /// Installs the assets as version `tag` (for ffmpeg, `None`: the first
    /// asset's hash) and makes it current, keeping `previous`, or with no
    /// previous version (nothing usable was current), the newest other
    /// working version. Returns the version. On failure nothing of the new
    /// version is left.
    pub async fn run(
        &self,
        tag: Option<String>,
        previous: Option<&str>,
        report: Report<'_>,
    ) -> Result<String, CoreError> {
        let incoming = self.dir.join(INCOMING);
        remove_dir_if_exists(&incoming).map_err(io_error)?;
        std::fs::create_dir_all(&incoming).map_err(io_error)?;
        let result = self.run_in(&incoming, tag, previous, report).await;
        if let Err(e) = remove_dir_if_exists(&incoming) {
            tracing::warn!("could not clean {}: {e}", incoming.display());
        }
        result
    }

    async fn run_in(
        &self,
        incoming: &Path,
        tag: Option<String>,
        previous: Option<&str>,
        report: Report<'_>,
    ) -> Result<String, CoreError> {
        let mut downloads = Vec::new();
        let mut done = 0;
        for asset in &self.assets {
            let name = asset.url.path_segments().and_then(|mut s| s.next_back());
            let file = incoming.join(name.filter(|n| !n.is_empty()).unwrap_or("asset"));
            let offset = done;
            let (sha256, size) = download(self.client, asset, &file, &|n, total| {
                report(offset + n, total.map(|t| offset + t));
            })
            .await?;
            done += size;
            downloads.push(Downloaded {
                file,
                sha256,
                layout: asset.layout.clone(),
            });
        }
        let version = tag.unwrap_or_else(|| hash_version(&downloads[0].sha256));
        if !is_version_name(&version) {
            return Err(failed(format!(
                "the published version {version:?} is not a usable folder name"
            )));
        }
        let names: Vec<&str> = self
            .assets
            .iter()
            .flat_map(|a| a.layout.install_names())
            .collect();
        let stage = incoming.join("stage");
        let target = self.dir.join(&version);
        {
            let (stage, target) = (stage.clone(), target.clone());
            tokio::task::spawn_blocking(move || place(downloads, &stage, &target))
                .await
                .map_err(|e| failed(format!("unpacking stopped: {e}")))??;
        }
        let activated = async {
            for name in &names {
                run_version(&target.join(name), self.version_arg, self.version_timeout).await?;
            }
            write_current(&self.dir, &version).map_err(io_error)
        };
        if let Err(e) = activated.await {
            if let Err(e) = remove_dir_if_exists(&target) {
                tracing::warn!("could not remove {}: {e}", target.display());
            }
            return Err(e);
        }
        let previous = previous
            .map(str::to_owned)
            .or_else(|| other_version(&self.dir, Some(&version), names[0]));
        prune(&self.dir, &[Some(version.as_str()), previous.as_deref()]);
        Ok(version)
    }
}

/// Unpacks every download into `stage`, then moves `stage` to `target`
/// (replacing a leftover folder of that version). Always new files, never
/// rewritten in place: macOS kills a process whose signed binary changed.
fn place(downloads: Vec<Downloaded>, stage: &Path, target: &Path) -> Result<(), CoreError> {
    std::fs::create_dir(stage).map_err(io_error)?;
    for d in &downloads {
        unpack(&d.file, &d.layout, stage)?;
    }
    remove_dir_if_exists(target).map_err(io_error)?;
    std::fs::rename(stage, target).map_err(io_error)
}

fn unpack(file: &Path, layout: &Layout, stage: &Path) -> Result<(), CoreError> {
    match layout {
        Layout::RawBinary { install_name } => {
            let path = stage.join(install_name);
            std::fs::rename(file, &path).map_err(io_error)?;
            set_executable(&path).map_err(io_error)
        }
        Layout::Zip { members } => {
            let unreadable = |e: zip::result::ZipError| {
                failed(format!("unreadable archive {}: {e}", file.display()))
            };
            let reader = BufReader::new(File::open(file).map_err(io_error)?);
            let mut archive = zip::ZipArchive::new(reader).map_err(unreadable)?;
            let mut found = vec![false; members.len()];
            for i in 0..archive.len() {
                let mut entry = archive.by_index(i).map_err(unreadable)?;
                if !entry.is_file() {
                    continue;
                }
                let name = entry.name().map_err(unreadable)?.replace('\\', "/");
                if let Some(k) = member_index(&name, members, &found) {
                    write_member(&mut entry, &stage.join(members[k].1))?;
                    found[k] = true;
                }
            }
            all_found(file, members, &found)
        }
        Layout::TarXz { members } => {
            let unreadable =
                |e: io::Error| failed(format!("unreadable archive {}: {e}", file.display()));
            let reader = BufReader::new(File::open(file).map_err(io_error)?);
            let mut archive = tar::Archive::new(lzma_rust2::XzReader::new(reader, true));
            let mut found = vec![false; members.len()];
            for entry in archive.entries().map_err(unreadable)? {
                let mut entry = entry.map_err(unreadable)?;
                if !entry.header().entry_type().is_file() {
                    continue;
                }
                let name = String::from_utf8_lossy(&entry.path_bytes()).replace('\\', "/");
                if let Some(k) = member_index(&name, members, &found) {
                    write_member(&mut entry, &stage.join(members[k].1))?;
                    found[k] = true;
                }
            }
            all_found(file, members, &found)
        }
    }
}

/// The first member not yet found whose path suffix `name` ends with (at a
/// path component boundary).
fn member_index(name: &str, members: &[(&str, &str)], found: &[bool]) -> Option<usize> {
    members.iter().zip(found).position(|(&(suffix, _), &done)| {
        !done
            && (name == suffix
                || name
                    .strip_suffix(suffix)
                    .is_some_and(|rest| rest.ends_with('/')))
    })
}

fn all_found(file: &Path, members: &[(&str, &str)], found: &[bool]) -> Result<(), CoreError> {
    match members.iter().zip(found).find(|(_, done)| !**done) {
        None => Ok(()),
        Some((&(suffix, _), _)) => Err(failed(format!("{} holds no {suffix}", file.display()))),
    }
}

fn write_member(reader: &mut impl Read, path: &Path) -> Result<(), CoreError> {
    let mut out = File::create_new(path).map_err(io_error)?;
    io::copy(reader, &mut out).map_err(io_error)?;
    drop(out);
    set_executable(path).map_err(io_error)
}

fn set_executable(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

/// Downloads `asset` to the new file `file`, checking its SHA-256 against
/// the published one. Returns the hash and the size.
async fn download(
    client: &reqwest::Client,
    asset: &Asset,
    file: &Path,
    report: Report<'_>,
) -> Result<(String, u64), CoreError> {
    let published = match &asset.checksum {
        ChecksumSource::SidecarOfResolved => None,
        source => Some(published_hash(client, source).await?),
    };
    let mut response = get(client, asset.url.clone()).await?;
    let expected = match published {
        Some(hash) => hash,
        // Wherever the redirects led, the hash sits beside that file.
        None => sidecar_hash(client, sidecar_of(response.url())).await?,
    };
    let total = response.content_length();
    let mut out = tokio::fs::File::create_new(file).await.map_err(io_error)?;
    let mut hasher = Sha256::new();
    let (mut size, mut reported) = (0u64, 0u64);
    while let Some(chunk) = response.chunk().await.map_err(network_error)? {
        hasher.update(&chunk);
        out.write_all(&chunk).await.map_err(io_error)?;
        size += chunk.len() as u64;
        if size - reported >= REPORT_STEP {
            report(size, total);
            reported = size;
        }
    }
    out.flush().await.map_err(io_error)?;
    drop(out);
    report(size, total);
    let actual = hex::encode(hasher.finalize());
    if actual != expected {
        return Err(CoreError {
            kind: ErrorKind::ChecksumMismatch,
            detail: format!(
                "SHA-256 of {} is {actual}, the publisher lists {expected}",
                asset.url
            ),
        });
    }
    Ok((actual, size))
}

async fn published_hash(
    client: &reqwest::Client,
    source: &ChecksumSource,
) -> Result<String, CoreError> {
    match source {
        ChecksumSource::SumsFile { url, name } => {
            let text = get_text(client, url.clone()).await?;
            parse_sums(&text, name).ok_or_else(|| CoreError {
                kind: ErrorKind::ChecksumMismatch,
                detail: format!("{url} lists no SHA-256 for {name}"),
            })
        }
        ChecksumSource::SidecarFile(url) => sidecar_hash(client, url.clone()).await,
        ChecksumSource::SidecarOfResolved => unreachable!("needs the resolved URL"),
    }
}

async fn sidecar_hash(client: &reqwest::Client, url: Url) -> Result<String, CoreError> {
    let text = get_text(client, url.clone()).await?;
    parse_sidecar(&text).ok_or_else(|| CoreError {
        kind: ErrorKind::ChecksumMismatch,
        detail: format!("{url} holds no SHA-256"),
    })
}

/// `<url>.sha256`, keeping any query.
fn sidecar_of(url: &Url) -> Url {
    let mut sidecar = url.clone();
    sidecar.set_path(&format!("{}.sha256", url.path()));
    sidecar
}

/// The `tag_name` of the GitHub release at `url`.
pub(super) async fn latest_tag(client: &reqwest::Client, url: Url) -> Result<String, CoreError> {
    #[derive(serde::Deserialize)]
    struct Release {
        tag_name: String,
    }
    let response = client
        .get(url.clone())
        .header(ACCEPT, "application/vnd.github+json")
        .send()
        .await
        .map_err(network_error)?;
    let text = checked(response)?.text().await.map_err(network_error)?;
    let release: Release = serde_json::from_str(&text)
        .map_err(|e| failed(format!("unreadable release information at {url}: {e}")))?;
    Ok(release.tag_name.trim().to_owned())
}

async fn get(client: &reqwest::Client, url: Url) -> Result<reqwest::Response, CoreError> {
    checked(client.get(url).send().await.map_err(network_error)?)
}

async fn get_text(client: &reqwest::Client, url: Url) -> Result<String, CoreError> {
    get(client, url).await?.text().await.map_err(network_error)
}

/// A publisher that answers 404 or 410 no longer publishes that build
/// (`ToolMissing`; Intel macOS ffmpeg builds end in 2027, for one); any
/// other failure status is taken as the network's (`ConnectionLost`).
fn checked(response: reqwest::Response) -> Result<reqwest::Response, CoreError> {
    let status = response.status();
    let kind = match status.as_u16() {
        200..=299 => return Ok(response),
        404 | 410 => ErrorKind::ToolMissing,
        _ => ErrorKind::ConnectionLost,
    };
    Err(CoreError {
        kind,
        detail: format!("{} answered HTTP {status}", response.url()),
    })
}

fn network_error(e: reqwest::Error) -> CoreError {
    CoreError {
        kind: ErrorKind::ConnectionLost,
        ..request_error(e)
    }
}

/// Runs `<path> <arg>`, which must exit 0 within `timeout`; a binary that
/// takes longer is killed (and reaped) before this returns.
async fn run_version(path: &Path, arg: &str, timeout: Duration) -> Result<(), CoreError> {
    let mut child = spawn_version(path, arg).await?;
    let mut stderr = child.stderr.take().expect("stderr is piped");
    let finished = tokio::time::timeout(timeout, async {
        let mut text = Vec::new();
        // A read error only loses the message.
        let (status, _) = tokio::join!(child.wait(), stderr.read_to_end(&mut text));
        (status, text)
    })
    .await;
    let (status, stderr) = match finished {
        Err(_) => {
            if let Err(e) = child.kill().await {
                tracing::warn!("could not stop {}: {e}", path.display());
            }
            return Err(failed(format!(
                "{} {arg} did not answer within {} s",
                path.display(),
                timeout.as_secs_f32()
            )));
        }
        Ok((status, stderr)) => (status, stderr),
    };
    let status =
        status.map_err(|e| failed(format!("could not wait for {}: {e}", path.display())))?;
    if status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&stderr);
    Err(failed(format!(
        "{} {arg} failed ({status}): {}",
        path.display(),
        stderr.lines().next().unwrap_or("").trim()
    )))
}

/// Starts `<path> <arg>`, retrying while exec refuses it as busy.
async fn spawn_version(path: &Path, arg: &str) -> Result<tokio::process::Child, CoreError> {
    let mut attempt = 0;
    loop {
        let mut command = tokio::process::Command::new(path);
        command
            .arg(arg)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(windows)]
        {
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            command.creation_flags(CREATE_NO_WINDOW);
        }
        match command.spawn() {
            Err(e) if is_busy(&e) && attempt + 1 < BUSY_ATTEMPTS => {
                attempt += 1;
                tokio::time::sleep(Duration::from_millis(25) * attempt).await;
            }
            Err(e) => return Err(failed(format!("could not run {}: {e}", path.display()))),
            Ok(child) => return Ok(child),
        }
    }
}

/// Linux refuses to run a binary still open for writing (ETXTBSY, 26 on
/// Linux and macOS), which a child forked by another thread may briefly hold.
fn is_busy(e: &io::Error) -> bool {
    cfg!(unix) && e.raw_os_error() == Some(26)
}

fn hash_version(sha256: &str) -> String {
    sha256[..HASH_VERSION_LEN.min(sha256.len())].to_owned()
}

/// A version is used as a folder name: plain characters only.
fn is_version_name(version: &str) -> bool {
    !version.is_empty()
        && version.len() <= 100
        && !version.starts_with('.')
        && version
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '+'))
}

/// The version `dir/current` names, if it is a plausible one.
pub(super) fn read_current(dir: &Path) -> Option<String> {
    read_marker(dir, CURRENT_FILE)
}

/// Atomically makes `version` current (a new file renamed over the old).
pub(super) fn write_current(dir: &Path, version: &str) -> io::Result<()> {
    let temp = dir.join(format!("{CURRENT_FILE}.new"));
    let _ = std::fs::remove_file(&temp);
    std::fs::write(&temp, format!("{version}\n"))?;
    std::fs::rename(&temp, dir.join(CURRENT_FILE))
}

/// The version a marker file in `dir` names (see `SKIP_FILE`), if plausible.
pub(super) fn read_marker(dir: &Path, name: &str) -> Option<String> {
    let text = std::fs::read_to_string(dir.join(name)).ok()?;
    let version = text.trim();
    is_version_name(version).then(|| version.to_owned())
}

pub(super) fn write_marker(dir: &Path, name: &str, version: &str) -> io::Result<()> {
    std::fs::write(dir.join(name), format!("{version}\n"))
}

pub(super) fn remove_marker(dir: &Path, name: &str) {
    match std::fs::remove_file(dir.join(name)) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => {
            tracing::warn!("could not remove {}: {e}", dir.join(name).display());
        }
        _ => {}
    }
}

/// The newest version folder other than `current` that holds `binary`.
pub(super) fn other_version(dir: &Path, current: Option<&str>, binary: &str) -> Option<String> {
    std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            let usable = is_version_name(&name)
                && Some(name.as_str()) != current
                && entry.path().join(binary).is_file();
            let modified = entry.metadata().and_then(|m| m.modified()).ok()?;
            usable.then_some((modified, name))
        })
        .max()
        .map(|(_, name)| name)
}

/// Deletes every version folder but those in `keep`. A folder still in use
/// (Windows) stays until a later install.
fn prune(dir: &Path, keep: &[Option<&str>]) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
        if !is_dir || name.starts_with('.') || keep.contains(&Some(name.as_str())) {
            continue;
        }
        if let Err(e) = std::fs::remove_dir_all(entry.path()) {
            tracing::warn!("could not remove {}: {e}", entry.path().display());
        }
    }
}

fn remove_dir_if_exists(path: &Path) -> io::Result<()> {
    match std::fs::remove_dir_all(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

fn failed(detail: String) -> CoreError {
    CoreError {
        kind: ErrorKind::ToolFailed,
        detail,
    }
}

pub(super) fn io_error(e: io::Error) -> CoreError {
    CoreError {
        kind: classify_io(&e),
        detail: e.to_string(),
    }
}
