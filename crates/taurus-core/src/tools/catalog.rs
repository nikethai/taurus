//! Where each tool's build for a platform is published, how its checksum
//! is published, and which files of it are installed. Pure: no I/O.

use url::Url;

use crate::errors::{CoreError, ErrorKind};

use super::{ToolKind, ToolSources};

/// One file to download for a tool.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Asset {
    pub url: Url,
    pub checksum: ChecksumSource,
    pub layout: Layout,
}

/// Where the SHA-256 of an asset is published.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChecksumSource {
    /// A `sha256sum`-style file listing many assets; `name` is this one's.
    SumsFile { url: Url, name: String },
    /// A file holding only this asset's hash (in any text around it).
    SidecarFile(Url),
    /// The asset URL redirects; the hash is at `<final URL>.sha256`.
    SidecarOfResolved,
}

/// What is installed from a downloaded asset.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Layout {
    /// The download is the binary itself.
    RawBinary { install_name: &'static str },
    /// `(path suffix in the archive, installed file name)` pairs.
    Zip {
        members: &'static [(&'static str, &'static str)],
    },
    TarXz {
        members: &'static [(&'static str, &'static str)],
    },
}

impl Layout {
    /// The installed file names, in order.
    pub fn install_names(&self) -> Vec<&'static str> {
        match self {
            Layout::RawBinary { install_name } => vec![install_name],
            Layout::Zip { members } | Layout::TarXz { members } => {
                members.iter().map(|&(_, name)| name).collect()
            }
        }
    }
}

const FFMPEG_UNIX: &[(&str, &str)] = &[("bin/ffmpeg", "ffmpeg"), ("bin/ffprobe", "ffprobe")];
const FFMPEG_WINDOWS: &[(&str, &str)] = &[
    ("bin/ffmpeg.exe", "ffmpeg.exe"),
    ("bin/ffprobe.exe", "ffprobe.exe"),
];
const DENO_UNIX: &[(&str, &str)] = &[("deno", "deno")];
const DENO_WINDOWS: &[(&str, &str)] = &[("deno.exe", "deno.exe")];
const MACOS_FFMPEG: &[(&str, &str)] = &[("ffmpeg", "ffmpeg")];
const MACOS_FFPROBE: &[(&str, &str)] = &[("ffprobe", "ffprobe")];

/// The assets that make up `kind` on `os`/`arch` (`std::env::consts`
/// spellings; `musl` for a musl Linux), in order: the first holds the
/// tool's main binary. macOS ffmpeg comes as two assets (ffmpeg, ffprobe).
/// No build for the platform is `ToolMissing`.
pub fn asset_for(
    kind: ToolKind,
    os: &str,
    arch: &str,
    musl: bool,
    sources: &ToolSources,
) -> Result<Vec<Asset>, CoreError> {
    let missing = || CoreError {
        kind: ErrorKind::ToolMissing,
        detail: format!("no build for {os}/{arch}"),
    };
    let aarch64 = match arch {
        "x86_64" => false,
        "aarch64" => true,
        _ => return Err(missing()),
    };
    let assets = match (kind, os) {
        (ToolKind::YtDlp, "linux" | "windows" | "macos") => {
            let name = match (os, aarch64, musl) {
                ("linux", false, false) => "yt-dlp_linux",
                ("linux", true, false) => "yt-dlp_linux_aarch64",
                ("linux", false, true) => "yt-dlp_musllinux",
                ("linux", true, true) => "yt-dlp_musllinux_aarch64",
                ("windows", false, _) => "yt-dlp.exe",
                ("windows", true, _) => "yt-dlp_arm64.exe",
                _ => "yt-dlp_macos",
            };
            let install_name = if os == "windows" {
                "yt-dlp.exe"
            } else {
                "yt-dlp"
            };
            vec![Asset {
                url: join(&sources.yt_dlp, name),
                checksum: ChecksumSource::SumsFile {
                    url: join(&sources.yt_dlp, "SHA2-256SUMS"),
                    name: name.to_owned(),
                },
                layout: Layout::RawBinary { install_name },
            }]
        }
        // Deno links against glibc.
        (ToolKind::Deno, "linux") if musl => return Err(missing()),
        (ToolKind::Deno, "linux" | "windows" | "macos") => {
            let triple_os = match os {
                "linux" => "unknown-linux-gnu",
                "windows" => "pc-windows-msvc",
                _ => "apple-darwin",
            };
            let name = format!("deno-{arch}-{triple_os}.zip");
            let url = join(&sources.deno, &name);
            vec![Asset {
                checksum: ChecksumSource::SidecarFile(join(
                    &sources.deno,
                    &format!("{name}.sha256sum"),
                )),
                url,
                layout: Layout::Zip {
                    members: if os == "windows" {
                        DENO_WINDOWS
                    } else {
                        DENO_UNIX
                    },
                },
            }]
        }
        (ToolKind::Ffmpeg, "linux" | "windows") => {
            let platform = match (os, aarch64) {
                ("linux", false) => "linux64",
                ("linux", true) => "linuxarm64",
                (_, false) => "win64",
                (_, true) => "winarm64",
            };
            let (extension, layout) = if os == "linux" {
                (
                    "tar.xz",
                    Layout::TarXz {
                        members: FFMPEG_UNIX,
                    },
                )
            } else {
                (
                    "zip",
                    Layout::Zip {
                        members: FFMPEG_WINDOWS,
                    },
                )
            };
            let name = format!("ffmpeg-master-latest-{platform}-gpl.{extension}");
            vec![Asset {
                url: join(&sources.ffmpeg_builds, &name),
                checksum: ChecksumSource::SumsFile {
                    url: join(&sources.ffmpeg_builds, "checksums.sha256"),
                    name,
                },
                layout,
            }]
        }
        (ToolKind::Ffmpeg, "macos") => {
            let dir = format!("{}/release/", if aarch64 { "arm64" } else { "amd64" });
            [("ffmpeg.zip", MACOS_FFMPEG), ("ffprobe.zip", MACOS_FFPROBE)]
                .into_iter()
                .map(|(name, members)| Asset {
                    url: join(&sources.ffmpeg_macos, &format!("{dir}{name}")),
                    checksum: ChecksumSource::SidecarOfResolved,
                    layout: Layout::Zip { members },
                })
                .collect()
        }
        _ => return Err(missing()),
    };
    Ok(assets)
}

/// `base` must be a directory URL (ending in '/').
fn join(base: &Url, path: &str) -> Url {
    base.join(path)
        .expect("a relative path joins onto a directory URL")
}

/// The hash listed for `name` in a `sha256sum`-style file (`<hex>  <name>`
/// or `<hex> *<name>`), lower-cased.
pub fn parse_sums(text: &str, name: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let mut tokens = line.split_whitespace();
        let hash = tokens.next()?;
        let file = tokens.next()?;
        (file.trim_start_matches('*') == name && tokens.next().is_none())
            .then(|| sha256_token(hash))
            .flatten()
    })
}

/// The first SHA-256 (64 hex digits) in a sidecar file, lower-cased: any
/// text around it is ignored (Deno's Windows sidecars are PowerShell
/// output with an upper-case hash).
pub fn parse_sidecar(text: &str) -> Option<String> {
    text.split_whitespace().find_map(sha256_token)
}

fn sha256_token(token: &str) -> Option<String> {
    (token.len() == 64 && token.bytes().all(|b| b.is_ascii_hexdigit()))
        .then(|| token.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::errors::ErrorKind;
    use crate::tools::{ToolKind, ToolSources};

    fn one(kind: ToolKind, os: &str, arch: &str, musl: bool) -> Asset {
        let mut assets = asset_for(kind, os, arch, musl, &ToolSources::default()).unwrap();
        assert_eq!(assets.len(), 1, "{assets:?}");
        assets.remove(0)
    }

    const YT: &str = "https://github.com/yt-dlp/yt-dlp/releases/latest/download/";

    #[test]
    fn asset_names_per_platform() {
        for (os, arch, musl, name, install) in [
            ("linux", "x86_64", false, "yt-dlp_linux", "yt-dlp"),
            ("linux", "aarch64", false, "yt-dlp_linux_aarch64", "yt-dlp"),
            ("linux", "x86_64", true, "yt-dlp_musllinux", "yt-dlp"),
            (
                "linux",
                "aarch64",
                true,
                "yt-dlp_musllinux_aarch64",
                "yt-dlp",
            ),
            ("windows", "x86_64", false, "yt-dlp.exe", "yt-dlp.exe"),
            (
                "windows",
                "aarch64",
                false,
                "yt-dlp_arm64.exe",
                "yt-dlp.exe",
            ),
            ("macos", "x86_64", false, "yt-dlp_macos", "yt-dlp"),
            ("macos", "aarch64", false, "yt-dlp_macos", "yt-dlp"),
        ] {
            let asset = one(ToolKind::YtDlp, os, arch, musl);
            assert_eq!(asset.url.as_str(), format!("{YT}{name}"));
            assert_eq!(
                asset.checksum,
                ChecksumSource::SumsFile {
                    url: url::Url::parse(&format!("{YT}SHA2-256SUMS")).unwrap(),
                    name: name.to_owned(),
                }
            );
            assert_eq!(
                asset.layout,
                Layout::RawBinary {
                    install_name: install
                }
            );
        }

        let deno = one(ToolKind::Deno, "windows", "x86_64", false);
        let deno_url = "https://github.com/denoland/deno/releases/latest/download/\
                        deno-x86_64-pc-windows-msvc.zip";
        assert_eq!(deno.url.as_str(), deno_url);
        assert_eq!(
            deno.checksum,
            ChecksumSource::SidecarFile(url::Url::parse(&format!("{deno_url}.sha256sum")).unwrap())
        );
        assert_eq!(
            deno.layout,
            Layout::Zip {
                members: &[("deno.exe", "deno.exe")]
            }
        );
        assert!(
            one(ToolKind::Deno, "macos", "aarch64", false)
                .url
                .as_str()
                .ends_with("/deno-aarch64-apple-darwin.zip")
        );
        assert!(
            one(ToolKind::Deno, "linux", "x86_64", false)
                .url
                .as_str()
                .ends_with("/deno-x86_64-unknown-linux-gnu.zip")
        );

        let builds = "https://github.com/yt-dlp/FFmpeg-Builds/releases/latest/download/";
        let ffmpeg = one(ToolKind::Ffmpeg, "linux", "aarch64", false);
        assert_eq!(
            ffmpeg.url.as_str(),
            format!("{builds}ffmpeg-master-latest-linuxarm64-gpl.tar.xz")
        );
        assert_eq!(
            ffmpeg.checksum,
            ChecksumSource::SumsFile {
                url: url::Url::parse(&format!("{builds}checksums.sha256")).unwrap(),
                name: "ffmpeg-master-latest-linuxarm64-gpl.tar.xz".to_owned(),
            }
        );
        assert_eq!(
            ffmpeg.layout,
            Layout::TarXz {
                members: &[("bin/ffmpeg", "ffmpeg"), ("bin/ffprobe", "ffprobe")]
            }
        );
        let win = one(ToolKind::Ffmpeg, "windows", "x86_64", false);
        assert!(
            win.url
                .as_str()
                .ends_with("/ffmpeg-master-latest-win64-gpl.zip")
        );
        assert_eq!(
            win.layout,
            Layout::Zip {
                members: &[
                    ("bin/ffmpeg.exe", "ffmpeg.exe"),
                    ("bin/ffprobe.exe", "ffprobe.exe")
                ]
            }
        );

        let mac = asset_for(
            ToolKind::Ffmpeg,
            "macos",
            "x86_64",
            false,
            &ToolSources::default(),
        )
        .unwrap();
        let base = "https://ffmpeg.martin-riedl.de/redirect/latest/macos/amd64/release/";
        assert_eq!(mac.len(), 2);
        assert_eq!(mac[0].url.as_str(), format!("{base}ffmpeg.zip"));
        assert_eq!(mac[1].url.as_str(), format!("{base}ffprobe.zip"));
        assert_eq!(mac[0].checksum, ChecksumSource::SidecarOfResolved);
        assert_eq!(
            mac[1].layout,
            Layout::Zip {
                members: &[("ffprobe", "ffprobe")]
            }
        );
        let arm = asset_for(
            ToolKind::Ffmpeg,
            "macos",
            "aarch64",
            false,
            &ToolSources::default(),
        )
        .unwrap();
        assert!(arm[0].url.as_str().contains("/macos/arm64/release/"));
    }

    #[test]
    fn unsupported_platform() {
        for kind in ToolKind::ALL {
            let err =
                asset_for(kind, "freebsd", "x86_64", false, &ToolSources::default()).unwrap_err();
            assert_eq!(err.kind, ErrorKind::ToolMissing);
            assert_eq!(err.detail, "no build for freebsd/x86_64");
            let err =
                asset_for(kind, "linux", "riscv64", false, &ToolSources::default()).unwrap_err();
            assert_eq!(err.kind, ErrorKind::ToolMissing);
        }
        // Deno publishes no musl build.
        let err = asset_for(
            ToolKind::Deno,
            "linux",
            "x86_64",
            true,
            &ToolSources::default(),
        )
        .unwrap_err();
        assert_eq!(err.kind, ErrorKind::ToolMissing);
    }

    #[test]
    fn sources_under_a_base() {
        let base = url::Url::parse("http://127.0.0.1:9/").unwrap();
        let s = ToolSources::under(&base);
        assert_eq!(s.yt_dlp.as_str(), "http://127.0.0.1:9/yt-dlp/");
        assert_eq!(s.deno.as_str(), "http://127.0.0.1:9/deno/");
        assert_eq!(
            s.ffmpeg_builds.as_str(),
            "http://127.0.0.1:9/ffmpeg-builds/"
        );
        assert_eq!(s.ffmpeg_macos.as_str(), "http://127.0.0.1:9/ffmpeg-macos/");
        assert_eq!(s.github_api.as_str(), "http://127.0.0.1:9/api/");
        let asset = asset_for(ToolKind::YtDlp, "linux", "x86_64", false, &s).unwrap();
        assert_eq!(
            asset[0].url.as_str(),
            "http://127.0.0.1:9/yt-dlp/yt-dlp_linux"
        );
    }

    const HASH: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn checksum_files_parse() {
        let other = "f".repeat(64);
        let sums = format!("{other}  yt-dlp.exe\n{HASH}  yt-dlp\n{other}  yt-dlp_linux\n");
        assert_eq!(parse_sums(&sums, "yt-dlp").as_deref(), Some(HASH));
        assert_eq!(parse_sums(&sums, "yt-dlp_macos"), None);
        let binary_mode = format!("{} *a.zip\r\n", HASH.to_uppercase());
        assert_eq!(parse_sums(&binary_mode, "a.zip").as_deref(), Some(HASH));

        assert_eq!(
            parse_sidecar(&format!("{HASH}  deno.zip\n")).as_deref(),
            Some(HASH)
        );
        let powershell = format!(
            "\r\nAlgorithm : SHA256\r\nHash      : {}\r\nPath      : C:\\a\\deno.zip\r\n\r\n",
            HASH.to_uppercase()
        );
        assert_eq!(parse_sidecar(&powershell).as_deref(), Some(HASH));
        assert_eq!(parse_sidecar("not a hash"), None);
    }
}
