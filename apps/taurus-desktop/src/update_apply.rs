//! How this copy of Hyades was installed, and how a downloaded update
//! replaces it: the Windows installer runs itself, an AppImage file or a
//! macOS bundle is swapped in place, anything else (deb, rpm, a portable
//! copy, `cargo run`) is only told about the new version.

use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};

use taurus_core::update;

/// How the running copy was installed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InstallKind {
    /// cargo-packager's NSIS installer (an `uninstall.exe` next to the exe).
    WindowsInstaller,
    /// A macOS bundle; `app` is the `.app` folder.
    MacBundle { app: PathBuf },
    /// A Linux AppImage; `path` is the `.AppImage` file.
    AppImage { path: PathBuf },
    /// Nothing Hyades can update by itself.
    NotifyOnly,
}

/// What to start once the app has quit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Restart {
    /// The updated app (an AppImage file, or a bundle opened with `open`).
    Relaunch(PathBuf),
    /// The downloaded Windows installer, which relaunches the app itself.
    RunInstaller(PathBuf),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ApplyError {
    /// The folder the app lives in cannot be written.
    NotWritable,
    /// The update could not be put in place; the detail says why.
    Broken(String),
}

/// How the copy at `exe` was installed, on `os` (`std::env::consts::OS`),
/// with `appimage_env` the `APPIMAGE` variable an AppImage's runtime sets.
pub fn detect(exe: &Path, appimage_env: Option<&OsStr>, os: &str) -> InstallKind {
    let folder = exe.parent();
    match os {
        // cargo-packager's NSIS installer writes it next to the exe.
        "windows" if folder.is_some_and(|f| f.join("uninstall.exe").is_file()) => {
            InstallKind::WindowsInstaller
        }
        "macos" => match mac_bundle_of(exe) {
            Some(app) => InstallKind::MacBundle { app },
            None => InstallKind::NotifyOnly,
        },
        "linux" => match appimage_env.map(Path::new) {
            Some(path) if path.is_file() => InstallKind::AppImage {
                path: path.to_path_buf(),
            },
            _ => InstallKind::NotifyOnly,
        },
        _ => InstallKind::NotifyOnly,
    }
}

/// `X.app` when `exe` is `X.app/Contents/MacOS/<exe>`.
fn mac_bundle_of(exe: &Path) -> Option<PathBuf> {
    let macos = exe.parent()?;
    let contents = macos.parent()?;
    let app = contents.parent()?;
    let is_app = app.extension().is_some_and(|e| e == "app");
    (macos.file_name()? == "MacOS" && contents.file_name()? == "Contents" && is_app)
        .then(|| app.to_path_buf())
}

/// The manifest's platform key for this build installed as `kind`; `None`
/// when it cannot update itself.
pub fn platform_for(kind: &InstallKind) -> Option<String> {
    if *kind == InstallKind::NotifyOnly {
        return None;
    }
    let appimage = matches!(kind, InstallKind::AppImage { .. });
    update::platform_key(std::env::consts::OS, std::env::consts::ARCH, appimage)
}

/// Puts the `downloaded` update in place of the running copy (except the
/// Windows installer, which does that itself once started) and answers
/// what to start after quitting.
pub fn apply(kind: &InstallKind, downloaded: &Path) -> Result<Restart, ApplyError> {
    match kind {
        InstallKind::WindowsInstaller => Ok(Restart::RunInstaller(downloaded.to_path_buf())),
        InstallKind::AppImage { path } => {
            swap_appimage(path, downloaded)?;
            Ok(Restart::Relaunch(path.clone()))
        }
        InstallKind::MacBundle { app } => {
            install_bundle(app, downloaded)?;
            Ok(Restart::Relaunch(app.clone()))
        }
        InstallKind::NotifyOnly => Err(ApplyError::Broken(
            "this copy of Hyades cannot update itself".into(),
        )),
    }
}

/// The folder of `path` and `path`'s file name.
fn split(path: &Path) -> Result<(&Path, &OsStr), ApplyError> {
    match (path.parent(), path.file_name()) {
        (Some(folder), Some(name)) => Ok((folder, name)),
        _ => Err(broken(format!("{} has no folder", path.display()))),
    }
}

/// `.<name><suffix>` in `folder`: a hidden file next to the app.
fn hidden(folder: &Path, name: &OsStr, suffix: &str) -> PathBuf {
    let mut file = std::ffi::OsString::from(".");
    file.push(name);
    file.push(suffix);
    folder.join(file)
}

/// `NotWritable` unless a file can be created (and is removed) in `folder`.
fn check_writable(folder: &Path, name: &OsStr) -> Result<(), ApplyError> {
    let probe = hidden(folder, name, ".hyades-test");
    std::fs::File::create(&probe).map_err(|_| ApplyError::NotWritable)?;
    let _ = std::fs::remove_file(&probe);
    Ok(())
}

/// Copies `downloaded` next to the AppImage, makes it executable and
/// renames it over the AppImage (atomic: a crash leaves one or the other).
fn swap_appimage(path: &Path, downloaded: &Path) -> Result<(), ApplyError> {
    let (folder, name) = split(path)?;
    check_writable(folder, name)?;
    let new = hidden(folder, name, ".new");
    let result = (|| {
        std::fs::copy(downloaded, &new)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&new, std::fs::Permissions::from_mode(0o755))?;
        }
        // On disk before it takes the AppImage's name: a power loss must
        // not leave a truncated AppImage.
        std::fs::OpenOptions::new()
            .write(true)
            .open(&new)?
            .sync_all()?;
        std::fs::rename(&new, path)
    })();
    result.map_err(|e| {
        let _ = std::fs::remove_file(&new);
        broken(format!("replacing {}: {e}", path.display()))
    })
}

/// Unpacks the `.app.tar.gz` next to the bundle and swaps it in.
fn install_bundle(app: &Path, downloaded: &Path) -> Result<(), ApplyError> {
    let (parent, name) = split(app)?;
    check_writable(parent, name)?;
    let temp = parent.join(format!(".hyades-update-{}", std::process::id()));
    let result = unpack_bundle(downloaded, &temp).and_then(|new_app| swap_bundle(app, &new_app));
    let _ = std::fs::remove_dir_all(&temp);
    result
}

/// Unpacks the gzip'd tar `archive` into `dest` (made afresh) and answers
/// the `Hyades.app` in it, which must hold `Contents/MacOS/hyades`.
fn unpack_bundle(archive: &Path, dest: &Path) -> Result<PathBuf, ApplyError> {
    let _ = std::fs::remove_dir_all(dest);
    let unpacked = (|| {
        std::fs::create_dir_all(dest)?;
        let file = std::fs::File::open(archive)?;
        let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(file));
        tar.set_preserve_permissions(true);
        // `unpack` keeps every entry inside `dest`.
        tar.unpack(dest)
    })();
    unpacked.map_err(|e| broken(format!("unpacking {}: {e}", archive.display())))?;
    let new_app = dest.join("Hyades.app");
    if !new_app
        .join("Contents")
        .join("MacOS")
        .join("hyades")
        .is_file()
    {
        return Err(broken("the update holds no Hyades.app".into()));
    }
    Ok(new_app)
}

/// Swaps the bundle at `app` for `new_app`: the old one is moved aside,
/// the new one takes its path, and the old one is deleted. If the new one
/// cannot take its place the old one is put back (`Broken`).
pub fn swap_bundle(app: &Path, new_app: &Path) -> Result<(), ApplyError> {
    swap_bundle_with(app, new_app, |from, to| std::fs::rename(from, to))
}

/// [`swap_bundle`] with the renames made by `rename` (a test seam).
fn swap_bundle_with(
    app: &Path,
    new_app: &Path,
    rename: impl Fn(&Path, &Path) -> io::Result<()>,
) -> Result<(), ApplyError> {
    let (parent, _) = split(app)?;
    let old = parent.join(format!(".Hyades-old-{}.app", std::process::id()));
    let _ = std::fs::remove_dir_all(&old);
    rename(app, &old).map_err(|e| broken(format!("moving {} aside: {e}", app.display())))?;
    if let Err(e) = rename(new_app, app) {
        let restored = rename(&old, app);
        return Err(broken(match restored {
            Ok(()) => format!("moving the new bundle in: {e}"),
            Err(r) => format!(
                "moving the new bundle in: {e}; putting {} back: {r}",
                app.display()
            ),
        }));
    }
    let _ = std::fs::remove_dir_all(&old);
    Ok(())
}

/// Starts `restart`, detached; the caller exits right after.
pub fn spawn(restart: Restart) -> io::Result<()> {
    use std::process::Command;
    match restart {
        // Passive, closes a copy still running, relaunches when done.
        Restart::RunInstaller(installer) if cfg!(windows) => {
            Command::new(installer).args(["/P", "/R"]).spawn()?;
        }
        Restart::RunInstaller(installer) => {
            return Err(io::Error::other(format!(
                "{} is a Windows installer",
                installer.display()
            )));
        }
        // A new instance even if the old one is still known to Launch
        // Services.
        Restart::Relaunch(app) if cfg!(target_os = "macos") => {
            Command::new("open").arg("-n").arg(app).spawn()?;
        }
        Restart::Relaunch(path) => {
            Command::new(path).spawn()?;
        }
    }
    Ok(())
}

fn broken(detail: String) -> ApplyError {
    ApplyError::Broken(detail)
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn detect_windows_installer() {
        let tmp = tempfile::tempdir().unwrap();
        let exe = tmp.path().join("hyades.exe");
        write(&exe, "exe");
        assert_eq!(detect(&exe, None, "windows"), InstallKind::NotifyOnly);
        write(&tmp.path().join("uninstall.exe"), "nsis");
        assert_eq!(detect(&exe, None, "windows"), InstallKind::WindowsInstaller);
    }

    #[test]
    fn detect_mac_bundle() {
        let app = Path::new("/Applications/Hyades.app");
        let exe = app.join("Contents").join("MacOS").join("hyades");
        assert_eq!(
            detect(&exe, None, "macos"),
            InstallKind::MacBundle {
                app: app.to_path_buf()
            }
        );
        // Not in Contents/MacOS: a copy run from elsewhere.
        let loose = app.join("Contents").join("hyades");
        assert_eq!(detect(&loose, None, "macos"), InstallKind::NotifyOnly);
        assert_eq!(
            detect(Path::new("/usr/local/bin/hyades"), None, "macos"),
            InstallKind::NotifyOnly
        );
    }

    #[test]
    fn detect_appimage() {
        let tmp = tempfile::tempdir().unwrap();
        let image = tmp.path().join("Hyades.AppImage");
        let exe = Path::new("/tmp/.mount_HyadesX/usr/bin/hyades");
        // Set but not an existing file: not trusted.
        assert_eq!(
            detect(exe, Some(image.as_os_str()), "linux"),
            InstallKind::NotifyOnly
        );
        write(&image, "image");
        assert_eq!(
            detect(exe, Some(image.as_os_str()), "linux"),
            InstallKind::AppImage { path: image }
        );
    }

    #[test]
    fn detect_notify_only() {
        let exe = Path::new("/usr/bin/hyades");
        assert_eq!(detect(exe, None, "linux"), InstallKind::NotifyOnly);
        assert_eq!(platform_for(&InstallKind::NotifyOnly), None);
    }

    #[cfg(unix)]
    #[test]
    fn appimage_swap_replaces_file_and_keeps_exec_bit() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("apps").join("Hyades.AppImage");
        write(&path, "old");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        let downloaded = tmp.path().join("updates").join("Hyades-1.1.0.AppImage");
        write(&downloaded, "new");
        let kind = InstallKind::AppImage { path: path.clone() };
        assert_eq!(
            apply(&kind, &downloaded),
            Ok(Restart::Relaunch(path.clone()))
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755);
        let left: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(left, [OsStr::new("Hyades.AppImage")], "nothing else left");
    }

    #[cfg(unix)]
    #[test]
    fn unwritable_appimage_falls_back() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let folder = tmp.path().join("apps");
        let path = folder.join("Hyades.AppImage");
        write(&path, "old");
        let downloaded = tmp.path().join("Hyades-1.1.0.AppImage");
        write(&downloaded, "new");
        std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o555)).unwrap();
        let probe = std::fs::write(folder.join("probe"), "");
        let result = apply(&InstallKind::AppImage { path: path.clone() }, &downloaded);
        std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o755)).unwrap();
        if probe.is_ok() {
            // Root ignores the mode bits, so the 0o555 folder cannot be
            // checked here. Say so, and check a folder root cannot write
            // to either; elsewhere fail instead of passing unchecked.
            eprintln!(
                "unwritable_appimage_falls_back: the 0o555 folder is writable \
                 (running as root); checking /proc instead"
            );
            #[cfg(target_os = "linux")]
            {
                let in_proc = InstallKind::AppImage {
                    path: PathBuf::from("/proc/Hyades.AppImage"),
                };
                assert_eq!(apply(&in_proc, &downloaded), Err(ApplyError::NotWritable));
                return;
            }
            #[cfg(not(target_os = "linux"))]
            panic!("cannot make a folder unwritable for this user (root?)");
        }
        assert_eq!(result, Err(ApplyError::NotWritable));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "old");
    }

    /// `<root>/<side>/Hyades.app` holding `Contents/MacOS/hyades` and a
    /// marker file reading `marker`.
    fn fake_bundle(root: &Path, side: &str, marker: &str) -> PathBuf {
        let app = root.join(side).join("Hyades.app");
        write(&app.join("Contents").join("MacOS").join("hyades"), "exe");
        write(&app.join("Contents").join("marker"), marker);
        app
    }

    fn marker(app: &Path) -> String {
        std::fs::read_to_string(app.join("Contents").join("marker")).unwrap()
    }

    fn old_bundles(dir: &Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with(".Hyades-old-"))
            .collect()
    }

    #[test]
    fn bundle_swap_replaces_bundle() {
        let tmp = tempfile::tempdir().unwrap();
        let old = fake_bundle(tmp.path(), "Old", "old");
        let new = fake_bundle(tmp.path(), "New", "new");
        swap_bundle(&old, &new).unwrap();
        assert_eq!(marker(&old), "new");
        assert!(!new.exists(), "the new bundle moved");
        assert_eq!(old_bundles(old.parent().unwrap()), Vec::<String>::new());
    }

    #[test]
    fn bundle_swap_restores_old_on_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let old = fake_bundle(tmp.path(), "Old", "old");
        let new = fake_bundle(tmp.path(), "New", "new");
        let calls = Cell::new(0);
        // The second rename (new bundle into place) finds a non-empty
        // folder at its target, as a stale copy would leave.
        let rename = |from: &Path, to: &Path| {
            calls.set(calls.get() + 1);
            if calls.get() != 2 {
                return std::fs::rename(from, to);
            }
            write(&to.join("in-the-way"), "stale");
            let result = std::fs::rename(from, to);
            std::fs::remove_dir_all(to).unwrap();
            assert!(result.is_err(), "renamed over a non-empty folder");
            result
        };
        let result = swap_bundle_with(&old, &new, rename);
        assert!(matches!(result, Err(ApplyError::Broken(_))), "{result:?}");
        assert_eq!(marker(&old), "old", "the old bundle is back");
        assert_eq!(marker(&new), "new");
        assert_eq!(old_bundles(old.parent().unwrap()), Vec::<String>::new());
    }

    /// A `.app.tar.gz` as the release publishes it, unpacked and swapped
    /// in; one without the executable is refused and changes nothing.
    #[test]
    fn mac_bundle_apply_untars_and_swaps() {
        let tmp = tempfile::tempdir().unwrap();
        let old = fake_bundle(tmp.path(), "Applications", "old");
        let src = fake_bundle(tmp.path(), "build", "new");
        let archive = |name: &str, with_exe: bool| {
            let path = tmp.path().join(name);
            let gz = flate2::write::GzEncoder::new(
                std::fs::File::create(&path).unwrap(),
                flate2::Compression::fast(),
            );
            let mut tar = tar::Builder::new(gz);
            if with_exe {
                tar.append_dir_all("Hyades.app", &src).unwrap();
            } else {
                tar.append_path_with_name(src.join("Contents").join("marker"), "Hyades.app/marker")
                    .unwrap();
            }
            tar.into_inner().unwrap().finish().unwrap();
            path
        };
        let kind = InstallKind::MacBundle { app: old.clone() };

        let broken = apply(&kind, &archive("broken.tar.gz", false));
        assert!(matches!(broken, Err(ApplyError::Broken(_))), "{broken:?}");
        assert_eq!(marker(&old), "old");

        let good = archive("Hyades.app.tar.gz", true);
        assert_eq!(apply(&kind, &good), Ok(Restart::Relaunch(old.clone())));
        assert_eq!(marker(&old), "new");
        let parent = old.parent().unwrap();
        let left: Vec<String> = std::fs::read_dir(parent)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(left, ["Hyades.app"], "no temporary folder left");
    }

    #[test]
    fn windows_installer_runs_itself() {
        let file = Path::new("/updates/Hyades-1.1.0-setup.exe");
        assert_eq!(
            apply(&InstallKind::WindowsInstaller, file),
            Ok(Restart::RunInstaller(file.to_path_buf()))
        );
    }
}
