//! Where the engine keeps its data: `HYADES_DATA_DIR`, else the older
//! `TAURUS_DATA_DIR`, else the operating system's data folder.

use std::ffi::OsString;
use std::path::PathBuf;

/// The operating system's data folder for the app (the `ProjectDirs` triple
/// `dev` / `nikethai` / `taurus`, which keeps existing data in place).
pub fn default_data_dir() -> Option<PathBuf> {
    directories::ProjectDirs::from("dev", "nikethai", "taurus")
        .map(|dirs| dirs.data_dir().to_path_buf())
}

/// The data folder from the two override variables (pass
/// `std::env::var_os` of each); an empty value counts as unset.
pub fn data_dir(hyades: Option<OsString>, taurus: Option<OsString>) -> Result<PathBuf, String> {
    hyades
        .filter(|d| !d.is_empty())
        .or_else(|| taurus.filter(|d| !d.is_empty()))
        .map(PathBuf::from)
        .or_else(default_data_dir)
        .ok_or_else(|| "no data directory found; set HYADES_DATA_DIR".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hyades_data_dir_wins() {
        let d = data_dir(Some("/h".into()), Some("/t".into())).unwrap();
        assert_eq!(d, PathBuf::from("/h"));
    }
    #[test]
    fn taurus_data_dir_still_read() {
        assert_eq!(
            data_dir(None, Some("/t".into())).unwrap(),
            PathBuf::from("/t")
        );
        assert_eq!(
            data_dir(Some("".into()), Some("/t".into())).unwrap(),
            PathBuf::from("/t")
        );
    }
    #[test]
    fn default_when_neither_set() {
        assert_eq!(data_dir(None, None).ok(), default_data_dir());
    }
}
