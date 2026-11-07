//! `cargo xtask i18n extract | pseudo | check`: the desktop app's
//! translation template (`.pot`) and the two generated catalogs, `en`
//! (empty, so the source text shows) and `en_XA` (the pseudo-locale).

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::po::{self, Entry};

const APP: &str = "apps/taurus-desktop";
const POT: &str = "apps/taurus-desktop/translations/taurus-desktop.pot";
const EXTRACTOR: &str = "slint-tr-extractor";
const INSTALL_HINT: &str = "install it with: cargo install --locked slint-tr-extractor@1.18.1";

/// The text a generated catalog gives a source string.
type Translate = fn(&str) -> String;

/// The generated catalogs: `en` is empty (the source text shows), `en_XA`
/// the pseudo-locale.
const CATALOGS: [(&str, Translate); 2] = [("en", |_| String::new()), ("en_XA", po::pseudo)];

pub fn run(args: &[String]) -> Result<(), String> {
    let root = workspace_root();
    match args.first().map(String::as_str) {
        Some("extract") => {
            extract(&root, &root.join(POT))?;
            println!("wrote {POT}");
            Ok(())
        }
        Some("pseudo") => {
            for (lang, path) in write_catalogs(&root)? {
                println!("wrote {lang}: {}", path.display());
            }
            Ok(())
        }
        Some("check") => check(&root),
        _ => Err("usage: cargo xtask i18n <extract|pseudo|check>".into()),
    }
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask lives in the workspace root")
        .to_path_buf()
}

/// Every `.slint` file under the app's `ui/`, relative to the workspace root
/// with `/` separators, sorted: the `.pot` is the same on every machine.
fn slint_files(root: &Path) -> Result<Vec<String>, String> {
    fn walk(dir: &Path, rel: &str, out: &mut Vec<String>) -> Result<(), String> {
        let entries = std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        for entry in entries {
            let entry = entry.map_err(|e| format!("{}: {e}", dir.display()))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let path = entry.path();
            let rel = format!("{rel}/{name}");
            if path.is_dir() {
                walk(&path, &rel, out)?;
            } else if name.ends_with(".slint") {
                out.push(rel);
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    walk(&root.join(APP).join("ui"), &format!("{APP}/ui"), &mut out)?;
    out.sort();
    Ok(out)
}

/// Runs the Slint extractor over every `.slint` file into `out`.
fn extract(root: &Path, out: &Path) -> Result<(), String> {
    let files = slint_files(root)?;
    if let Some(dir) = out.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let status = Command::new(EXTRACTOR)
        .current_dir(root)
        .arg("-o")
        .arg(out)
        .args(&files)
        .status()
        .map_err(|e| format!("cannot run {EXTRACTOR} ({e}); {INSTALL_HINT}"))?;
    if !status.success() {
        return Err(format!("{EXTRACTOR} failed ({status})"));
    }
    // The extractor has no option to leave out `#: file:line`; without them
    // moving UI code changes nothing here.
    let text = read(out)?;
    std::fs::write(out, without_references(&text)).map_err(|e| format!("{}: {e}", out.display()))
}

/// `text` without its `#: file:line` reference comments.
fn without_references(text: &str) -> String {
    text.lines()
        .filter(|l| !l.starts_with("#:"))
        .map(|l| format!("{l}\n"))
        .collect()
}

/// The form `check` compares: no references and no `POT-Creation-Date`.
fn normalised(text: &str) -> String {
    without_references(text)
        .lines()
        .filter(|l| !l.starts_with("\"POT-Creation-Date:"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn catalog_path(root: &Path, lang: &str) -> PathBuf {
    root.join(APP)
        .join("translations")
        .join(lang)
        .join("LC_MESSAGES")
        .join("taurus-desktop.po")
}

fn read(path: &Path) -> Result<String, String> {
    std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))
}

/// Writes the `en` and `en_XA` catalogs from the committed `.pot`.
fn write_catalogs(root: &Path) -> Result<Vec<(&'static str, PathBuf)>, String> {
    let entries = po::parse_pot(&read(&root.join(POT))?);
    let mut written = Vec::new();
    for (lang, translate) in CATALOGS {
        let path = catalog_path(root, lang);
        let dir = path.parent().expect("catalog path has a folder");
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        std::fs::write(&path, po::write_po(&entries, lang, translate))
            .map_err(|e| format!("{}: {e}", path.display()))?;
        written.push((lang, path));
    }
    Ok(written)
}

/// Re-extracts into a temporary file and compares it with the committed
/// `.pot` (ignoring `POT-Creation-Date` and any `#:` references), then compares both catalogs with
/// what `pseudo` would write. Every difference is listed.
fn check(root: &Path) -> Result<(), String> {
    let fresh_path = std::env::temp_dir().join(format!("hyades-i18n-{}.pot", std::process::id()));
    let fresh = extract(root, &fresh_path).and_then(|()| read(&fresh_path));
    let _ = std::fs::remove_file(&fresh_path);
    let fresh = fresh?;
    let committed = read(&root.join(POT))?;

    let mut problems = Vec::new();
    diff(
        POT,
        &normalised(&fresh),
        &normalised(&committed),
        &mut problems,
    );
    let entries = po::parse_pot(&committed);
    for (lang, translate) in CATALOGS {
        let path = catalog_path(root, lang);
        let name = format!("{APP}/translations/{lang}/LC_MESSAGES/taurus-desktop.po");
        match read(&path) {
            Ok(actual) => diff(
                &name,
                &po::write_po(&entries, lang, translate),
                &actual,
                &mut problems,
            ),
            Err(e) => problems.push(e),
        }
    }
    if problems.is_empty() {
        println!("catalog up to date");
        return Ok(());
    }
    for p in &problems {
        eprintln!("{p}");
    }
    Err(
        "catalog out of date: run `cargo xtask i18n extract`, then `cargo xtask i18n pseudo`"
            .into(),
    )
}

fn key(e: &Entry) -> String {
    match &e.ctxt {
        Some(ctxt) => format!("{ctxt} | {}", e.id),
        None => e.id.clone(),
    }
}

/// Lists the differences between the expected and the committed text of
/// `name`: messages missing or stale, else the first line that differs.
fn diff(name: &str, expected: &str, actual: &str, problems: &mut Vec<String>) {
    if expected == actual {
        return;
    }
    let (want, have) = (po::parse_pot(expected), po::parse_pot(actual));
    let before = problems.len();
    for e in want.iter().filter(|e| !have.contains(e)) {
        problems.push(format!("{name}: missing {:?}", key(e)));
    }
    for e in have.iter().filter(|e| !want.contains(e)) {
        problems.push(format!("{name}: stale {:?}", key(e)));
    }
    if problems.len() == before {
        let mut lines = expected.lines().zip(actual.lines()).enumerate();
        let first = lines.find(|(_, (w, h))| w != h);
        problems.push(match first {
            Some((i, (w, h))) => format!("{name}:{}: expected {w:?}, found {h:?}", i + 1),
            None => format!("{name}: differs in length"),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const POT: &str = "\"POT-Creation-Date: 2026-11-06 13:15+0000\\n\"\n\n\
        #: apps/taurus-desktop/ui/welcome.slint:145\n\
        msgctxt \"Welcome\"\nmsgid \"Downloads made simple.\"\nmsgstr \"\"\n";

    #[test]
    fn moving_a_tr_line_keeps_the_normalised_pot() {
        let moved = POT
            .replace("welcome.slint:145", "welcome.slint:171")
            .replace("13:15", "14:02");
        assert_ne!(POT, moved);
        assert_eq!(normalised(POT), normalised(&moved));
        assert!(!without_references(POT).contains("#:"));
        assert!(without_references(POT).contains("msgctxt \"Welcome\"\n"));
        let reworded = POT.replace("made simple", "made easy");
        assert_ne!(normalised(POT), normalised(&reworded));
    }
}
