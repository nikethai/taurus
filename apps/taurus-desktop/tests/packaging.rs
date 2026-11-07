//! The cargo-packager configs in packaging/: each OS file repeats
//! packager-common.toml (cargo-packager reads one file per `--config`), and
//! the version they package is this crate's. The AppStream metainfo's
//! release is this crate's version too.

const METAINFO: &str = include_str!("../../../packaging/linux/dev.nikethai.hyades.metainfo.xml");
const COMMON: &str = include_str!("../../../packaging/packager-common.toml");
const OS_CONFIGS: [(&str, &str); 3] = [
    (
        "packager-linux.toml",
        include_str!("../../../packaging/packager-linux.toml"),
    ),
    (
        "packager-windows.toml",
        include_str!("../../../packaging/packager-windows.toml"),
    ),
    (
        "packager-macos.toml",
        include_str!("../../../packaging/packager-macos.toml"),
    ),
];

/// Lines that are neither blank nor comments.
fn values(toml: &str) -> impl Iterator<Item = &str> {
    toml.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
}

#[test]
fn os_configs_repeat_common_values_and_package_this_version() {
    let version = format!("version = \"{}\"", env!("CARGO_PKG_VERSION"));
    assert!(
        values(COMMON).any(|l| l == version),
        "packager-common.toml lacks `{version}`"
    );
    for (name, config) in OS_CONFIGS {
        // Top-level keys come before the first table.
        let top: Vec<&str> = values(config).take_while(|l| !l.starts_with('[')).collect();
        for line in values(COMMON) {
            assert!(
                top.contains(&line),
                "{name} lacks `{line}` from packager-common.toml"
            );
        }
    }
}

#[test]
fn metainfo_release_is_this_version() {
    let release = format!("<release version=\"{}\"", env!("CARGO_PKG_VERSION"));
    assert!(
        METAINFO.contains(&release),
        "dev.nikethai.hyades.metainfo.xml lacks `{release}` in <releases>"
    );
}
