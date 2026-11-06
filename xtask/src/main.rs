//! Workspace tooling, run as `cargo xtask <command>` (see .cargo/config.toml).
//!
//! - `i18n extract`: the desktop app's `.pot` from its `.slint` files.
//! - `i18n pseudo`: the `en` and `en_XA` catalogs from the `.pot`.
//! - `i18n check`: fails when the `.pot` or the catalogs are out of date.

mod i18n;
mod po;

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("i18n") => i18n::run(&args[1..]),
        _ => Err("usage: cargo xtask i18n <extract|pseudo|check>".into()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("xtask: {e}");
            ExitCode::FAILURE
        }
    }
}
