//! The codename must not show in anything a user reads: every `@tr(...)`
//! argument in the `.slint` files and every string literal of
//! `src/shell_text.rs`. Pure file scanning, so no Slint backend is needed.

use std::path::{Path, PathBuf};

fn slint_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            slint_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "slint") {
            out.push(path);
        }
    }
}

/// Every double-quoted literal in `text` (backslash escapes skipped).
fn literals(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur: Option<String> = None;
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        match (&mut cur, c) {
            (Some(s), '\\') => {
                s.push(c);
                s.extend(chars.next());
            }
            (Some(_), '"') => out.extend(cur.take()),
            (Some(s), _) => s.push(c),
            (None, '"') => cur = Some(String::new()),
            (None, _) => {}
        }
    }
    out
}

/// The text of each `@tr( ... )` call: from the `(` to its matching `)`.
fn tr_args(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(i) = rest.find("@tr(") {
        let body = &rest[i + 4..];
        let (mut depth, mut in_str, mut esc, mut end) = (1, false, false, body.len());
        for (j, c) in body.char_indices() {
            if in_str {
                match (esc, c) {
                    (true, _) => esc = false,
                    (false, '\\') => esc = true,
                    (false, '"') => in_str = false,
                    _ => {}
                }
            } else if c == '"' {
                in_str = true;
            } else if c == '(' {
                depth += 1;
            } else if c == ')' {
                depth -= 1;
                if depth == 0 {
                    end = j;
                    break;
                }
            }
        }
        out.push(body[..end].to_string());
        rest = &body[end..];
    }
    out
}

#[test]
fn no_codename_in_user_text() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    slint_files(&root.join("ui"), &mut files);
    assert!(!files.is_empty(), "no .slint files found");
    let mut seen = 0;
    for file in &files {
        let text = std::fs::read_to_string(file).unwrap();
        for arg in tr_args(&text) {
            seen += 1;
            assert!(
                !arg.to_lowercase().contains("taurus"),
                "{}: codename in @tr({arg})",
                file.display()
            );
        }
    }
    assert!(seen > 0, "no @tr calls found");
    let shell = std::fs::read_to_string(root.join("src/shell_text.rs")).unwrap();
    // Only the part before the unit tests: they may name the codename.
    let shell = shell.split("#[cfg(test)]").next().unwrap();
    for lit in literals(shell) {
        assert!(
            !lit.to_lowercase().contains("taurus"),
            "codename in shell_text literal {lit:?}"
        );
    }
}
