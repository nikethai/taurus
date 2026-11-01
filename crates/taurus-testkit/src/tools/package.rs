//! Publisher asset names and containers mirrored by `FakeToolHost`: every
//! asset holds the native fake-tool binary, whatever platform it names.

use std::io::{Cursor, Write};

use sha2::{Digest, Sha256};

/// yt-dlp release assets (raw binaries), listed in `SHA2-256SUMS`.
pub(super) const YT_DLP_ASSETS: [&str; 7] = [
    "yt-dlp.exe",
    "yt-dlp_arm64.exe",
    "yt-dlp_macos",
    "yt-dlp_linux",
    "yt-dlp_linux_aarch64",
    "yt-dlp_musllinux",
    "yt-dlp_musllinux_aarch64",
];

/// Deno targets: asset `deno-<triple>.zip` with `deno` / `deno.exe` at its root.
pub(super) const DENO_TRIPLES: [&str; 6] = [
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu",
    "x86_64-apple-darwin",
    "aarch64-apple-darwin",
    "x86_64-pc-windows-msvc",
    "aarch64-pc-windows-msvc",
];

/// yt-dlp/FFmpeg-Builds platforms: `ffmpeg-master-latest-<plat>-gpl.{tar.xz,zip}`.
pub(super) const FFMPEG_BUILDS: [&str; 4] = ["linux64", "linuxarm64", "win64", "winarm64"];

/// martin-riedl macOS architectures and the zips each one offers.
pub(super) const MACOS_ARCHS: [&str; 2] = ["arm64", "amd64"];
pub(super) const MACOS_ZIPS: [&str; 2] = ["ffmpeg.zip", "ffprobe.zip"];

/// Last 16 bytes of a binary carrying a release trailer.
pub(super) const TRAILER_MAGIC: &[u8; 16] = b"HYADES-FAKETOOL1";

/// `<binary><payload><payload len: u32 LE><TRAILER_MAGIC>`, payload
/// `version=<version>\nbroken=<0|1>\n`.
pub(super) fn append_trailer(binary: &mut Vec<u8>, version: &str, broken: bool) {
    assert!(
        !version.contains(['\n', '\r']),
        "fake tool version must be one line"
    );
    let payload = format!("version={version}\nbroken={}\n", u8::from(broken));
    binary.extend_from_slice(payload.as_bytes());
    binary.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    binary.extend_from_slice(TRAILER_MAGIC);
}

pub(super) fn deno_asset(triple: &str) -> String {
    format!("deno-{triple}.zip")
}

pub(super) fn ffmpeg_build_asset(plat: &str) -> String {
    let ext = if plat.starts_with("win") {
        "zip"
    } else {
        "tar.xz"
    };
    format!("ffmpeg-master-latest-{plat}-gpl.{ext}")
}

/// The zip for one Deno target.
pub(super) fn deno_zip(triple: &str, binary: &[u8]) -> Vec<u8> {
    let name = if triple.contains("windows") {
        "deno.exe"
    } else {
        "deno"
    };
    zip(&[(name, Some(binary))])
}

/// One martin-riedl zip: the program alone at the archive root.
pub(super) fn macos_zip(zip_name: &str, binary: &[u8]) -> Vec<u8> {
    zip(&[(zip_name.trim_end_matches(".zip"), Some(binary))])
}

/// An FFmpeg-Builds archive: `<root>/bin/ffmpeg[.exe]`, `<root>/bin/ffprobe[.exe]`
/// and a licence file, with directory entries, like the real ones.
pub(super) fn ffmpeg_build(plat: &str, binary: &[u8]) -> Vec<u8> {
    let root = format!("ffmpeg-master-latest-{plat}-gpl");
    let exe = if plat.starts_with("win") { ".exe" } else { "" };
    let names = [
        format!("{root}/"),
        format!("{root}/bin/"),
        format!("{root}/bin/ffmpeg{exe}"),
        format!("{root}/bin/ffprobe{exe}"),
        format!("{root}/LICENSE.txt"),
    ];
    let licence = b"GNU GENERAL PUBLIC LICENSE (fake)\n".as_slice();
    let entries: [(&str, Option<&[u8]>); 5] = [
        (&names[0], None),
        (&names[1], None),
        (&names[2], Some(binary)),
        (&names[3], Some(binary)),
        (&names[4], Some(licence)),
    ];
    if plat.starts_with("win") {
        zip(&entries)
    } else {
        tar_xz(&entries)
    }
}

/// A deflated zip; `None` content is a directory entry (name ends in `/`).
fn zip(entries: &[(&str, Option<&[u8]>)]) -> Vec<u8> {
    use zip::write::SimpleFileOptions;
    use zip::{CompressionMethod, DateTime};

    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .compression_level(Some(1))
        .last_modified_time(DateTime::default());
    for &(name, content) in entries {
        match content {
            None => writer
                .add_directory(name, options.unix_permissions(0o755))
                .expect("zip directory"),
            Some(bytes) => {
                writer
                    .start_file(name, options.unix_permissions(mode(name)))
                    .expect("zip entry");
                writer.write_all(bytes).expect("zip write");
            }
        }
    }
    writer.finish().expect("zip finish").into_inner()
}

/// Directories and programs are executable; the licence text is not.
fn mode(name: &str) -> u32 {
    if name.ends_with(".txt") { 0o644 } else { 0o755 }
}

/// A ustar archive compressed as .xz; `None` content is a directory.
fn tar_xz(entries: &[(&str, Option<&[u8]>)]) -> Vec<u8> {
    let mut tar = tar::Builder::new(Vec::new());
    for &(name, content) in entries {
        let mut header = tar::Header::new_ustar();
        header.set_mode(mode(name));
        header.set_mtime(0);
        match content {
            None => {
                header.set_entry_type(tar::EntryType::Directory);
                header.set_size(0);
                tar.append_data(&mut header, name, std::io::empty())
                    .expect("tar dir");
            }
            Some(bytes) => {
                header.set_entry_type(tar::EntryType::Regular);
                header.set_size(bytes.len() as u64);
                tar.append_data(&mut header, name, bytes).expect("tar file");
            }
        }
    }
    let tar = tar.into_inner().expect("tar finish");
    let options = lzma_rust2::XzOptions::with_preset(1);
    let mut xz = lzma_rust2::XzWriter::new(Vec::new(), options).expect("xz writer");
    xz.write_all(&tar).expect("xz write");
    xz.finish().expect("xz finish")
}

/// The SHA-256 a checksum file publishes: the real one, or (corrupt) a
/// well-formed hash of something else.
pub(super) fn published_sha256(bytes: &[u8], corrupt: bool) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    if corrupt {
        hasher.update(b"corrupt");
    }
    hex::encode(hasher.finalize())
}

/// sha256sum format: one `<hex>  <name>` line per file.
pub(super) fn sums_file(entries: &[(String, String)]) -> String {
    entries
        .iter()
        .map(|(hex, name)| format!("{hex}  {name}\n"))
        .collect()
}

/// Deno's Windows sidecars are PowerShell `Get-FileHash | Format-List`
/// output: CRLF lines, upper-case hash, the CI runner's path.
pub(super) fn powershell_sidecar(hex: &str, name: &str) -> String {
    format!(
        "\r\nAlgorithm : SHA256\r\nHash      : {}\r\nPath      : C:\\a\\deno\\deno\\target\\release\\{name}\r\n\r\n",
        hex.to_uppercase()
    )
}
