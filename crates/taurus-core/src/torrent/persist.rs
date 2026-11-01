//! librqbit's own torrent list, pruned before the session starts. librqbit
//! restores every torrent in it, opening (and creating) its files, before the
//! hub can reconcile; so a torrent no job runs any more must be gone from it
//! first, or its deleted or moved files come back. The store stays the
//! source of truth: what is pruned too much, the reconcile adds back.
//!
//! The list is `session.json` in the session folder; beside it each torrent
//! has `<info hash>.torrent` (its metainfo) and `<info hash>.bitv` (its
//! fastresume bitfield).

use std::collections::HashSet;
use std::io::ErrorKind;
use std::path::Path;

use serde_json::{Map, Value};

const SESSION_FILE: &str = "session.json";
/// Where an unreadable `SESSION_FILE` is moved, so the session starts empty.
const UNREADABLE_FILE: &str = "session.json.unreadable";

/// Keeps in the list in `folder` only the torrents whose info hash
/// (lowercase hex) is in `keep`, and deletes the metainfo and fastresume
/// files of every other torrent. An entry librqbit could not read is
/// dropped; a list that cannot be read at all is moved aside, so the session
/// starts empty instead of failing. An error writing the list is returned.
pub(super) fn prune(folder: &Path, keep: &HashSet<String>) -> std::io::Result<()> {
    let path = folder.join(SESSION_FILE);
    let kept = match std::fs::read(&path) {
        Ok(bytes) => match pruned(&bytes, keep) {
            Some((list, kept, changed)) => {
                if changed {
                    write_atomically(folder, &path, &list)?;
                }
                kept
            }
            None => {
                tracing::warn!(
                    file = %path.display(),
                    "the torrent session's list is unreadable; starting it empty"
                );
                std::fs::rename(&path, folder.join(UNREADABLE_FILE))?;
                HashSet::new()
            }
        },
        Err(e) if e.kind() == ErrorKind::NotFound => HashSet::new(),
        Err(e) => return Err(e),
    };
    remove_strays(folder, &kept)
}

/// The list with only the kept, readable entries (one per info hash), the
/// info hashes kept, and whether anything was dropped; `None` if the list
/// itself cannot be read.
fn pruned(bytes: &[u8], keep: &HashSet<String>) -> Option<(Vec<u8>, HashSet<String>, bool)> {
    let Value::Object(mut root) = serde_json::from_slice(bytes).ok()? else {
        return None;
    };
    let Some(Value::Object(torrents)) = root.remove("torrents") else {
        return None;
    };
    let before = torrents.len();
    let mut kept = HashSet::new();
    let mut list = Map::new();
    for (id, entry) in torrents {
        if id.parse::<usize>().is_err() {
            continue;
        }
        if let Some(hash) = readable_hash(&entry)
            && keep.contains(&hash)
            && kept.insert(hash)
        {
            list.insert(id, entry);
        }
    }
    let changed = list.len() != before || !root.is_empty();
    let mut out = Map::new();
    out.insert("torrents".to_string(), Value::Object(list));
    let bytes = serde_json::to_vec(&Value::Object(out)).ok()?;
    Some((bytes, kept, changed))
}

/// The entry's info hash, if librqbit 9.1 can read the entry: one bad entry
/// makes it refuse the whole list.
fn readable_hash(entry: &Value) -> Option<String> {
    let hash = entry.get("info_hash")?.as_str()?;
    let strings = |v: &Value| v.as_array().is_some_and(|a| a.iter().all(Value::is_string));
    let indices = |v: &Value| {
        v.is_null()
            || v.as_array()
                .is_some_and(|a| a.iter().all(|i| i.as_u64().is_some()))
    };
    let readable = is_hash(hash)
        && entry.get("trackers").is_some_and(strings)
        && entry.get("output_folder").is_some_and(Value::is_string)
        && entry.get("only_files").is_some_and(indices)
        && entry.get("is_paused").is_some_and(Value::is_boolean);
    readable.then(|| hash.to_string())
}

fn is_hash(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn write_atomically(folder: &Path, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = folder.join(format!("{SESSION_FILE}.prune"));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

/// Deletes the `<hash>.torrent` and `<hash>.bitv` files (and a half-written
/// `<hash>.bitv.tmp`) of every torrent not in `kept`: a fastresume bitfield
/// left behind would tell a torrent added again that it has pieces it no
/// longer has.
fn remove_strays(folder: &Path, kept: &HashSet<String>) -> std::io::Result<()> {
    let entries = match std::fs::read_dir(folder) {
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(()),
        entries => entries?,
    };
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let hash = [".torrent", ".bitv", ".bitv.tmp"]
            .iter()
            .find_map(|ext| name.strip_suffix(ext))
            .filter(|h| is_hash(h));
        if let Some(hash) = hash
            && !kept.contains(hash)
            && let Err(e) = std::fs::remove_file(entry.path())
            && e.kind() != ErrorKind::NotFound
        {
            tracing::warn!(file = name, "could not delete a stale torrent file: {e}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn entry(hash: &str) -> Value {
        serde_json::json!({
            "info_hash": hash,
            "trackers": ["udp://t.example:1337/announce"],
            "output_folder": "/out/x",
            "only_files": [0, 2],
            "is_paused": false,
        })
    }

    fn files(folder: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(folder)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn keeps_only_the_store_torrents() {
        let dir = tempfile::tempdir().unwrap();
        let (keep, gone, garbled) = ("a".repeat(40), "b".repeat(40), "c".repeat(40));
        let mut bad = entry(&garbled);
        bad["is_paused"] = Value::from("yes");
        let list = serde_json::json!({ "torrents": {
            "0": entry(&keep),
            "1": entry(&gone),
            "2": bad,
            "3": entry(&keep),
            "x": entry(&keep),
        }});
        fs::write(dir.path().join(SESSION_FILE), list.to_string()).unwrap();
        for hash in [&keep, &gone, &garbled] {
            for ext in ["torrent", "bitv", "bitv.tmp"] {
                fs::write(dir.path().join(format!("{hash}.{ext}")), b"x").unwrap();
            }
        }
        fs::write(dir.path().join("dht.json"), b"{}").unwrap();

        let wanted = HashSet::from([keep.clone(), garbled.clone()]);
        prune(dir.path(), &wanted).unwrap();
        let list: Value =
            serde_json::from_slice(&fs::read(dir.path().join(SESSION_FILE)).unwrap()).unwrap();
        assert_eq!(
            list,
            serde_json::json!({ "torrents": { "0": entry(&keep) } })
        );
        let k = |ext: &str| format!("{keep}.{ext}");
        assert_eq!(
            files(dir.path()),
            [
                k("bitv"),
                k("bitv.tmp"),
                k("torrent"),
                "dht.json".to_string(),
                SESSION_FILE.to_string()
            ]
        );
        // Nothing more to drop: the list is left as it is.
        let before = fs::metadata(dir.path().join(SESSION_FILE)).unwrap();
        prune(dir.path(), &wanted).unwrap();
        let after = fs::metadata(dir.path().join(SESSION_FILE)).unwrap();
        assert_eq!(before.modified().unwrap(), after.modified().unwrap());
    }

    #[test]
    fn an_unreadable_list_is_moved_aside() {
        for content in ["{ not json", "[]", r#"{"torrents": []}"#] {
            let dir = tempfile::tempdir().unwrap();
            let hash = "d".repeat(40);
            fs::write(dir.path().join(SESSION_FILE), content).unwrap();
            fs::write(dir.path().join(format!("{hash}.bitv")), b"x").unwrap();
            prune(dir.path(), &HashSet::from([hash])).unwrap();
            assert_eq!(files(dir.path()), [UNREADABLE_FILE], "{content}");
            assert_eq!(
                fs::read_to_string(dir.path().join(UNREADABLE_FILE)).unwrap(),
                content
            );
        }
    }

    #[test]
    fn no_list_is_fine() {
        let dir = tempfile::tempdir().unwrap();
        prune(&dir.path().join("none"), &HashSet::new()).unwrap();
        fs::write(dir.path().join(format!("{}.torrent", "e".repeat(40))), b"x").unwrap();
        prune(dir.path(), &HashSet::new()).unwrap();
        assert!(files(dir.path()).is_empty());
    }
}
