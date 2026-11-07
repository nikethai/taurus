//! The window's language. The `.slint` strings are bundled with their
//! translations (build.rs, `translations/`); Slint picks one from the OS
//! locale when the window is made, and `apply` overrides it with
//! `HYADES_LANG` or the saved setting.

/// The language to select: `HYADES_LANG` wins, then the setting. `None`
/// leaves it to the OS locale. Empty strings count as unset.
pub fn choose(env: Option<&str>, setting: Option<&str>) -> Option<String> {
    [env, setting]
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|lang| !lang.is_empty())
        .map(str::to_owned)
}

/// Selects the bundled translation for `lang` (`en` is the source text),
/// trying the language alone when `vi_VN` or `vi-VN` is not bundled. A
/// language without a translation logs and keeps English. Call it after
/// the window exists.
pub fn apply(lang: Option<&str>) {
    let Some(lang) = lang else { return };
    if slint::select_bundled_translation(lang).is_ok() {
        return;
    }
    let base = lang.split(['_', '-', '.', '@']).next().unwrap_or(lang);
    if base != lang && slint::select_bundled_translation(base).is_ok() {
        return;
    }
    eprintln!("hyades: no translation for {lang}");
    let _ = slint::select_bundled_translation("en");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn choose_prefers_env_then_setting() {
        assert_eq!(choose(Some("en_XA"), Some("vi")).as_deref(), Some("en_XA"));
        assert_eq!(choose(None, Some("vi")).as_deref(), Some("vi"));
        assert_eq!(choose(Some(""), None), None);
    }
}
