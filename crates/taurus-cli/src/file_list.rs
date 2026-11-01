//! The `--files` list of `add`: file indices as `0,2,5-7`.

use std::str::FromStr;

/// Far beyond any real torrent's file count: keeps a huge range from
/// filling memory.
const MAX_FILE_INDEX: usize = 1_000_000;

/// File indices as `--files` takes them: `0,2,5-7`, sorted and deduplicated.
#[derive(Clone, Debug, PartialEq)]
pub struct FileList(pub Vec<usize>);

impl FromStr for FileList {
    type Err = String;

    fn from_str(text: &str) -> std::result::Result<FileList, String> {
        let invalid = |token: &str| format!("invalid file list item '{token}': use e.g. 0,2,5-7");
        let mut indices = Vec::new();
        for token in text.split(',').map(str::trim) {
            let number = |n: &str| {
                n.trim()
                    .parse::<usize>()
                    .ok()
                    .filter(|&n| n <= MAX_FILE_INDEX)
                    .ok_or_else(|| invalid(token))
            };
            match token.split_once('-') {
                Some((first, last)) => {
                    let (first, last) = (number(first)?, number(last)?);
                    if first > last {
                        return Err(invalid(token));
                    }
                    indices.extend(first..=last);
                }
                None => indices.push(number(token)?),
            }
        }
        indices.sort_unstable();
        indices.dedup();
        Ok(FileList(indices))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(text: &str) -> std::result::Result<Vec<usize>, String> {
        text.parse::<FileList>().map(|FileList(v)| v)
    }

    #[test]
    fn file_list_grammar() {
        assert_eq!(list("0,2,5-7"), Ok(vec![0, 2, 5, 6, 7]));
        assert_eq!(list(" 3 , 1 - 2 "), Ok(vec![1, 2, 3]));
        assert_eq!(list("4,4,3-4"), Ok(vec![3, 4]));
        assert_eq!(list("5-5"), Ok(vec![5]));
        assert_eq!(list("1000000"), Ok(vec![1_000_000]));
        let huge = format!("0-{}", usize::MAX);
        for bad in [
            "",
            "a",
            "7-5",
            "1,,2",
            "-2",
            "2-",
            "1-2-3",
            "1.5",
            ",",
            "1000001",
            "0-1000001",
            &huge,
        ] {
            assert!(list(bad).is_err(), "{bad:?}");
        }
    }
}
