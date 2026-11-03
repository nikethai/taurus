//! Compiles `src/bin/fake-tool.rs` (std only) into OUT_DIR with the same
//! rustc, so `fake_tool_bytes()` can embed it: `CARGO_BIN_EXE_*` exists only
//! in this crate's own integration tests, not in other crates' tests.

use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    let source = "src/bin/fake-tool.rs";
    println!("cargo:rerun-if-changed={source}");
    // The root file's `mod`s (a directory is scanned for changes).
    println!("cargo:rerun-if-changed=src/bin/fake_tool");
    println!("cargo:rerun-if-changed=build.rs");

    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR"));
    let windows = env::var("CARGO_CFG_TARGET_OS").is_ok_and(|os| os == "windows");
    let exe = out_dir.join(if windows {
        "fake-tool.exe"
    } else {
        "fake-tool"
    });
    let rustc = env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    let target = env::var("TARGET").expect("TARGET");
    let manifest_dir =
        PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));

    let status = Command::new(&rustc)
        .args(["--edition", "2021", "-O", "-C", "strip=debuginfo"])
        .args(["--crate-name", "fake_tool", "--crate-type", "bin"])
        .args(["--target", &target])
        .arg("-o")
        .arg(&exe)
        .arg(manifest_dir.join(source))
        .status()
        .unwrap_or_else(|e| panic!("cannot run {rustc:?}: {e}"));
    assert!(status.success(), "compiling {source} failed: {status}");
}
