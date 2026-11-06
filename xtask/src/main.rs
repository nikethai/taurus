//! Workspace tooling, run as `cargo xtask <command>` (see .cargo/config.toml).
//!
//! - `i18n extract`: the desktop app's `.pot` from its `.slint` files.
//! - `i18n pseudo`: the `en` and `en_XA` catalogs from the `.pot`.
//! - `i18n check`: fails when the `.pot` or the catalogs are out of date.
//! - `icons`: `packaging/icons/*` from the icon drawn in code.
//! - `keygen --out <dir>`: the update signing key pair.
//! - `sign <file>...`: minisign signatures (key from the environment).
//! - `manifest ...`: `latest.json` and `SHA256SUMS` for a release.

mod i18n;
mod icons;
mod po;
mod release;
mod sign;

use std::path::PathBuf;
use std::process::ExitCode;

/// The repository root (xtask lives one level below it).
pub(crate) fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..")
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("i18n") => i18n::run(&args[1..]),
        Some("icons") => icons::run(&args[1..]),
        Some("keygen") => sign::run_keygen(&args[1..]),
        Some("sign") => sign::run_sign(&args[1..]),
        Some("manifest") => release::run(&args[1..]),
        _ => Err("usage: cargo xtask <i18n|icons|keygen|sign|manifest> ...".into()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("xtask: {e}");
            ExitCode::FAILURE
        }
    }
}
