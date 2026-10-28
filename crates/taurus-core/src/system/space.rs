//! Whether a download fits on its disk: enough free space, and a file
//! system that can hold a file of its size.

use std::path::Path;

use crate::errors::{CoreError, ErrorKind};

/// Free space that must be left beyond the bytes still to download.
pub const SPACE_MARGIN: u64 = 16 * 1024 * 1024;
/// The largest file FAT32 can hold.
pub const FAT32_MAX_FILE: u64 = 4_294_967_295;

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
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{GetVolumeInformationW, GetVolumePathNameW};
    const MAX_PATH: usize = 261;
    let path: Vec<u16> = dir.as_os_str().encode_wide().chain([0]).collect();
    let mut root = [0u16; MAX_PATH];
    let mut fs_name = [0u16; MAX_PATH];
    // SAFETY: `path` is NUL-terminated; each buffer's length is passed with it.
    let found = unsafe {
        GetVolumePathNameW(path.as_ptr(), root.as_mut_ptr(), root.len() as u32) != 0
            && GetVolumeInformationW(
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

/// Checks that `remaining` more bytes fit in `dir`, and a file of `total`
/// bytes on its file system. Space that cannot be read does not block.
pub fn check_fits(dir: &Path, remaining: u64, total: Option<u64>) -> Result<(), CoreError> {
    match fs4::available_space(dir) {
        Ok(available) => check_fits_with(available, fs_kind(dir), remaining, total),
        Err(e) => {
            tracing::warn!(dir = %dir.display(), "could not read the free space: {e}");
            Ok(())
        }
    }
}

/// The rule behind [`check_fits`]: a FAT32 file system refuses a file over
/// 4 GiB - 1 byte; `available` must cover `remaining` plus 16 MiB.
pub fn check_fits_with(
    available: u64,
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
    if available < remaining.saturating_add(SPACE_MARGIN) {
        return Err(CoreError {
            kind: ErrorKind::DiskFull,
            detail: format!(
                "{remaining} bytes to download need {SPACE_MARGIN} more free; {available} bytes are free"
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
                FsKind::Other,
                90 * MIB,
                Some(90 * MIB)
            )),
            Err(ErrorKind::DiskFull)
        );
        assert_eq!(
            kind(check_fits_with(
                100 * MIB,
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
                FsKind::Fat32,
                5 * GIB,
                Some(5 * GIB)
            )),
            Err(ErrorKind::FileTooLargeForDisk)
        );
        assert_eq!(
            kind(check_fits_with(
                100 * GIB,
                FsKind::Fat32,
                1,
                Some(FAT32_MAX_FILE)
            )),
            Ok(())
        );
        assert_eq!(
            kind(check_fits_with(
                100 * GIB,
                FsKind::Other,
                5 * GIB,
                Some(5 * GIB)
            )),
            Ok(())
        );
        assert_eq!(
            kind(check_fits_with(100 * GIB, FsKind::Fat32, 5 * GIB, None)),
            Ok(()),
            "an unknown size is not refused"
        );
    }

    #[test]
    fn errors_name_the_numbers() {
        let full = check_fits_with(100, FsKind::Other, 200, Some(200)).unwrap_err();
        assert!(
            full.detail.contains("200") && full.detail.contains("100"),
            "{full}"
        );
        let big = check_fits_with(u64::MAX, FsKind::Fat32, 1, Some(5 * GIB)).unwrap_err();
        assert!(big.detail.contains(&(5 * GIB).to_string()), "{big}");
    }

    #[test]
    fn tempdir_is_not_fat32() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(fs_kind(dir.path()), FsKind::Other);
        // A missing folder cannot be told: not FAT32.
        assert_eq!(fs_kind(&dir.path().join("missing")), FsKind::Other);
        assert!(check_fits(dir.path(), 1, Some(1)).is_ok());
        assert_eq!(
            check_fits(dir.path(), u64::MAX - SPACE_MARGIN, None).map_err(|e| e.kind),
            Err(ErrorKind::DiskFull)
        );
    }
}
