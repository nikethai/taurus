//! Copied from xtask/src/po.rs (the gettext catalog parser): tests cannot
//! depend on the xtask binary crate. Keep the two in step.
//! Strings stay escaped as in the file.

/// One message of the catalog (the header, whose id is empty, is left out).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub ctxt: Option<String>,
    pub id: String,
    pub plural: Option<String>,
}

#[derive(Clone, Copy)]
enum Field {
    Ctxt,
    Id,
    Plural,
    Str(usize),
}

#[derive(Default)]
struct Raw {
    ctxt: Option<String>,
    id: Option<String>,
    plural: Option<String>,
    strs: Vec<String>,
}

impl Raw {
    fn slot(&mut self, field: Field) -> &mut String {
        match field {
            Field::Ctxt => self.ctxt.get_or_insert_default(),
            Field::Id => self.id.get_or_insert_default(),
            Field::Plural => self.plural.get_or_insert_default(),
            Field::Str(i) => {
                if self.strs.len() <= i {
                    self.strs.resize(i + 1, String::new());
                }
                &mut self.strs[i]
            }
        }
    }

    fn flush(&mut self, out: &mut Vec<(Entry, Vec<String>)>) {
        let raw = std::mem::take(self);
        if let Some(id) = raw.id.filter(|id| !id.is_empty()) {
            let (ctxt, plural) = (raw.ctxt, raw.plural);
            out.push((Entry { ctxt, id, plural }, raw.strs));
        }
    }
}

/// Every message with its `msgstr`s (one, or one per plural form).
pub fn parse(text: &str) -> Vec<(Entry, Vec<String>)> {
    let mut out = Vec::new();
    let mut raw = Raw::default();
    let mut last = Field::Id;
    for line in text.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('"') {
            raw.slot(last).push_str(unquote(line));
            continue;
        }
        let (key, value) = line.split_once(' ').unwrap_or((line, ""));
        let field = match key {
            "msgctxt" => Field::Ctxt,
            "msgid" => Field::Id,
            "msgid_plural" => Field::Plural,
            "msgstr" => Field::Str(0),
            _ => match key
                .strip_prefix("msgstr[")
                .and_then(|k| k.strip_suffix(']'))
            {
                Some(n) => Field::Str(n.parse().unwrap_or(0)),
                None => continue,
            },
        };
        let starts_entry = matches!(field, Field::Ctxt)
            || (matches!(field, Field::Id) && (raw.id.is_some() || !raw.strs.is_empty()));
        if starts_entry {
            raw.flush(&mut out);
        }
        raw.slot(field).push_str(unquote(value.trim()));
        last = field;
    }
    raw.flush(&mut out);
    out
}

fn unquote(s: &str) -> &str {
    let s = s.strip_prefix('"').unwrap_or(s);
    s.strip_suffix('"').unwrap_or(s)
}
