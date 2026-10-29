//! Whether a download fits on its disk: enough free space (beside what
//! other downloads to the same disk still need), and a file system that can
//! hold a file of its size.

use std::path::Path;

use crate::errors::{CoreError, ErrorKind};

/// Free space that must be left beyond the bytes still to download.
pub const SPACE_MARGIN: u64 = 16 * 1024 * 1024;
/// The largest file FAT32 can hold.
pub const FAT32_MAX_FILE: u64 = 4_294_967_295;

/// Reads the free space, in bytes, of the disk holding a folder.
pub type SpaceSource = fn(&Path) -> std::io::Result<u64>;

/// The free space of the disk holding `dir`, as the OS reports it to this
/// user.
pub fn available_space(dir: &Path) -> std::io::Result<u64> {
    fs4::available_space(dir)
}

/// Whether the two folders are on the same disk (volume). When that cannot
/// be told, only the same folder (compared canonicalized) counts.
pub(crate) fn same_volume(a: &Path, b: &Path) -> bool {
    match (volume_of(a), volume_of(b)) {
        (Some(a), Some(b)) => a == b,
        _ => canonical(a) == canonical(b),
    }
}

fn canonical(dir: &Path) -> std::path::PathBuf {
    dunce::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf())
}

#[cfg(unix)]
fn volume_of(dir: &Path) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(dir).ok().map(|m| m.dev())
}

/// The volume's root path (e.g. `c:\` or a mount folder), lower-cased.
#[cfg(windows)]
fn volume_of(dir: &Path) -> Option<String> {
    let root = volume_root(dir)?;
    let len = root.iter().position(|&c| c == 0).unwrap_or(root.len());
    Some(String::from_utf16_lossy(&root[..len]).to_lowercase())
}

#[cfg(not(any(unix, windows)))]
fn volume_of(_dir: &Path) -> Option<()> {
    None
}

#[cfg(windows)]
const MAX_PATH: usize = 261;

/// The NUL-terminated root path of the volume holding `dir`.
#[cfg(windows)]
fn volume_root(dir: &Path) -> Option<[u16; MAX_PATH]> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetVolumePathNameW;
    let path: Vec<u16> = dir.as_os_str().encode_wide().chain([0]).collect();
    let mut root = [0u16; MAX_PATH];
    // SAFETY: `path` is NUL-terminated; the buffer's length is passed with it.
    let found =
        unsafe { GetVolumePathNameW(path.as_ptr(), root.as_mut_ptr(), root.len() as u32) != 0 };
    found.then_some(root)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FsKind {
    Fat32,
    Other,
}

/// The kind of file system holding `dir`; `Other` when it cannot be told.
pub fn fs_kind(dir: &Path) -> FsKind {
    if is_fat(dir).unwrap_or(false) {
        FsKind::Fat32
    } else {
        FsKind::Other
    }
}

// `f_type`'s integer type differs between architectures.
#[allow(clippy::unnecessary_cast)]
#[cfg(target_os = "linux")]
fn is_fat(dir: &Path) -> Option<bool> {
    /// `MSDOS_SUPER_MAGIC`: vfat, msdos and FAT32 alike.
    const MSDOS_SUPER_MAGIC: i64 = 0x4d44;
    let stat = statfs(dir)?;
    Some(stat.f_type as i64 == MSDOS_SUPER_MAGIC)
}

#[cfg(target_os = "macos")]
fn is_fat(dir: &Path) -> Option<bool> {
    let stat = statfs(dir)?;
    // SAFETY: the kernel fills `f_fstypename` with a NUL-terminated name.
    let name = unsafe { std::ffi::CStr::from_ptr(stat.f_fstypename.as_ptr()) };
    Some(name.to_bytes() == b"msdos")
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn statfs(dir: &Path) -> Option<libc::statfs> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(dir.as_os_str().as_bytes()).ok()?;
    let mut stat = std::mem::MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: `path` is NUL-terminated and `stat` is written before it is read.
    let rc = unsafe { libc::statfs(path.as_ptr(), stat.as_mut_ptr()) };
    // SAFETY: `statfs` filled `stat` when it returned 0.
    (rc == 0).then(|| unsafe { stat.assume_init() })
}

#[cfg(windows)]
fn is_fat(dir: &Path) -> Option<bool> {
    use windows_sys::Win32::Storage::FileSystem::GetVolumeInformationW;
    let root = volume_root(dir)?;
    let mut fs_name = [0u16; MAX_PATH];
    // SAFETY: `root` is NUL-terminated; the buffer's length is passed with it.
    let found = unsafe {
        GetVolumeInformationW(
            root.as_ptr(),
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            fs_name.as_mut_ptr(),
            fs_name.len() as u32,
        ) != 0
    };
    if !found {
        return None;
    }
    let len = fs_name
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(fs_name.len());
    let name = String::from_utf16_lossy(&fs_name[..len]);
    Some(name == "FAT32" || name == "FAT")
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn is_fat(_dir: &Path) -> Option<bool> {
    None
}

/// Checks that `remaining` more bytes fit in `dir` beside the `reserved`
/// bytes other downloads to the same disk still need, and a file of `total`
/// bytes on its file system. Free space is read from `space`; space that
/// cannot be read does not block.
pub fn check_fits(
    space: SpaceSource,
    dir: &Path,
    reserved: u64,
    remaining: u64,
    total: Option<u64>,
) -> Result<(), CoreError> {
    match space(dir) {
        Ok(available) => check_fits_with(available, reserved, fs_kind(dir), remaining, total),
        Err(e) => {
            tracing::warn!(dir = %dir.display(), "could not read the free space: {e}");
            Ok(())
        }
    }
}

/// The rule behind [`check_fits`]: a FAT32 file system refuses a file over
/// 4 GiB - 1 byte; `available` less `reserved` must cover `remaining` plus
/// 16 MiB.
pub fn check_fits_with(
    available: u64,
    reserved: u64,
    fs: FsKind,
    remaining: u64,
    total: Option<u64>,
) -> Result<(), CoreError> {
    if fs == FsKind::Fat32
        && let Some(total) = total.filter(|&total| total > FAT32_MAX_FILE)
    {
        return Err(CoreError {
            kind: ErrorKind::FileTooLargeForDisk,
            detail: format!(
                "the file is {total} bytes; a FAT32 disk holds at most {FAT32_MAX_FILE} bytes per file"
            ),
        });
    }
    if available.saturating_sub(reserved) < remaining.saturating_add(SPACE_MARGIN) {
        let others = if reserved > 0 {
            format!(", {reserved} of them needed by other downloads")
        } else {
            String::new()
        };
        return Err(CoreError {
            kind: ErrorKind::DiskFull,
            detail: format!(
                "{remaining} bytes to download need {SPACE_MARGIN} more free; {available} bytes are free{others}"
            ),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1024 * 1024;
    const GIB: u64 = 1024 * MIB;

    #[test]
    fn fits_rules() {
        let kind = |r: Result<(), CoreError>| r.map_err(|e| e.kind);
        assert_eq!(
            kind(check_fits_with(
                100 * MIB,
                0,
                FsKind::Other,
                90 * MIB,
                Some(90 * MIB)
            )),
            Err(ErrorKind::DiskFull)
        );
        assert_eq!(
            kind(check_fits_with(
                100 * MIB,
                0,
                FsKind::Other,
                84 * MIB,
                Some(84 * MIB)
            )),
            Ok(()),
            "exactly the margin left"
        );
        assert_eq!(
            kind(check_fits_with(
                100 * GIB,
                0,
                FsKind::Fat32,
                5 * GIB,
                Some(5 * GIB)
            )),
            Err(ErrorKind::FileTooLargeForDisk)
        );
        assert_eq!(
            kind(check_fits_with(
                100 * GIB,
                0,
                FsKind::Fat32,
                1,
                Some(FAT32_MAX_FILE)
            )),
            Ok(())
        );
        assert_eq!(
            kind(check_fits_with(
                100 * GIB,
                0,
                FsKind::Other,
                5 * GIB,
                Some(5 * GIB)
            )),
            Ok(())
        );
        assert_eq!(
            kind(check_fits_with(100 * GIB, 0, FsKind::Fat32, 5 * GIB, None)),
            Ok(()),
            "an unknown size is not refused"
        );
    }

    #[test]
    fn errors_name_the_numbers() {
        let full = check_fits_with(100, 0, FsKind::Other, 200, Some(200)).unwrap_err();
        assert!(
            full.detail.contains("200") && full.detail.contains("100"),
            "{full}"
        );
        let big = check_fits_with(u64::MAX, 0, FsKind::Fat32, 1, Some(5 * GIB)).unwrap_err();
        assert!(big.detail.contains(&(5 * GIB).to_string()), "{big}");
    }

    #[test]
    fn tempdir_is_not_fat32() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(fs_kind(dir.path()), FsKind::Other);
        // A missing folder cannot be told: not FAT32.
        assert_eq!(fs_kind(&dir.path().join("missing")), FsKind::Other);
        assert!(check_fits(available_space, dir.path(), 0, 1, Some(1)).is_ok());
        assert_eq!(
            check_fits(
                available_space,
                dir.path(),
                0,
                u64::MAX - SPACE_MARGIN,
                None
            )
            .map_err(|e| e.kind),
            Err(ErrorKind::DiskFull)
        );
    }

    #[test]
    fn reserved_bytes_are_not_free() {
        let kind = |r: Result<(), CoreError>| r.map_err(|e| e.kind);
        // 100 MiB free, 84 MiB to go: fits with nothing reserved...
        assert_eq!(
            kind(check_fits_with(
                100 * MIB,
                0,
                FsKind::Other,
                84 * MIB,
                Some(84 * MIB)
            )),
            Ok(())
        );
        // ...not once another download still needs 1 byte of it.
        assert_eq!(
            kind(check_fits_with(
                100 * MIB,
                1,
                FsKind::Other,
                84 * MIB,
                Some(84 * MIB)
            )),
            Err(ErrorKind::DiskFull)
        );
        assert_eq!(
            kind(check_fits_with(
                100 * MIB,
                50 * MIB,
                FsKind::Other,
                34 * MIB,
                Some(34 * MIB)
            )),
            Ok(()),
            "exactly the margin left beside the reservation"
        );
        assert_eq!(
            kind(check_fits_with(
                100 * MIB,
                u64::MAX,
                FsKind::Other,
                0,
                Some(0)
            )),
            Err(ErrorKind::DiskFull),
            "more reserved than free"
        );
        let full = check_fits_with(100 * MIB, 60 * MIB, FsKind::Other, 30 * MIB, None).unwrap_err();
        let reserved = (60 * MIB).to_string();
        assert!(full.detail.contains(&reserved), "{full}");
    }

    #[test]
    fn same_volume_of_folders() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        assert!(same_volume(dir.path(), dir.path()));
        assert!(same_volume(dir.path(), &sub));
        let gone = dir.path().join("gone");
        assert!(same_volume(&gone, &gone));
        // Missing folders have no device to compare: only the same one counts.
        // (Windows names the volume of a missing path too.)
        #[cfg(unix)]
        assert!(!same_volume(&gone, &dir.path().join("other")));
    }
}
