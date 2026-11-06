//! Translation catalogs, checked without the Slint backend: the committed
//! pseudo-locale covers every string of the `.pot`, and the product name is
//! never sent for translation.

#[path = "common/po.rs"]
mod po;

use std::path::{Path, PathBuf};

fn root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn read(rel: &str) -> String {
    std::fs::read_to_string(root().join(rel)).unwrap_or_else(|e| panic!("{rel}: {e}"))
}

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

#[test]
fn pseudo_catalog_covers_every_string() {
    let pot = po::parse(&read("translations/taurus-desktop.pot"));
    let xa = po::parse(&read("translations/en_XA/LC_MESSAGES/taurus-desktop.po"));
    assert!(!pot.is_empty(), "no strings in the .pot");
    for (entry, _) in &pot {
        let key = (&entry.ctxt, &entry.id);
        let found = xa.iter().find(|(e, _)| (&e.ctxt, &e.id) == key);
        let Some((_, strs)) = found else {
            panic!("en_XA lacks {key:?}");
        };
        let forms = if entry.plural.is_some() { 2 } else { 1 };
        assert_eq!(strs.len(), forms, "en_XA {key:?}: {strs:?}");
        assert!(
            strs.iter().all(|s| !s.is_empty()),
            "en_XA {key:?} has an empty msgstr"
        );
    }
}

#[test]
fn product_name_is_not_translatable() {
    let mut files = Vec::new();
    slint_files(&root().join("ui"), &mut files);
    assert!(!files.is_empty(), "no .slint files found");
    for file in files {
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(
            !text.contains("@tr(\"Hyades\")"),
            "{}: the product name must be a plain literal",
            file.display()
        );
    }
}
