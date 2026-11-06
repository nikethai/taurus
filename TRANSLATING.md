# Translating Hyades

The desktop app's text lives in its `.slint` files, inside `@tr(...)`. Each
language is a gettext catalog compiled into the app:

```
apps/taurus-desktop/translations/
  taurus-desktop.pot                     template (generated, do not edit)
  en/LC_MESSAGES/taurus-desktop.po       English, empty (generated)
  en_XA/LC_MESSAGES/taurus-desktop.po    pseudo-locale (generated)
  <lang>/LC_MESSAGES/taurus-desktop.po   one per language, written by hand
```

## Adding a language

From `apps/taurus-desktop`, create the catalog from the template (Vietnamese
here; use the language's code as the folder name):

```
msginit -l vi -i translations/taurus-desktop.pot -o translations/vi/LC_MESSAGES/taurus-desktop.po
```

Fill in each `msgstr` with any `.po` editor (Poedit, Lokalize, a text
editor). An empty `msgstr` shows the English text. Keep the `msgctxt` lines:
they say which screen a string belongs to, and the same English word may need
different translations on different screens.

## Plural forms

Strings that depend on a number have `msgid_plural` and one `msgstr[N]` per
form. The header line `Plural-Forms` gives the count and the rule, for
example:

```
"Plural-Forms: nplurals=1; plural=0;\n"                 Vietnamese
"Plural-Forms: nplurals=2; plural=(n != 1);\n"          English, German
```

`msginit` fills it in for most languages; check it, since the app uses it to
pick the form.

## What not to translate

- `Hyades`, the product name. It stays as it is in every language.
- Placeholders: `{0}`, `{1}`, `{n}`, `{}`. Keep each one, exactly as written;
  you may move them within the sentence.
- Escapes such as `\n`.

## Trying it

Build and run the app with the language forced:

```
HYADES_LANG=vi cargo run -p taurus-desktop
```

Without `HYADES_LANG` the app uses the language saved in its settings, then
the system's. `HYADES_LANG=en_XA` shows the pseudo-locale: every translated
string is accented, longer and wrapped in `⟦…⟧`, so text that is not
translated, or that gets cut off, stands out.

## After changing UI text

Whenever `@tr(...)` text changes in a `.slint` file, regenerate the template
and the generated catalogs (this needs
`cargo install --locked slint-tr-extractor@1.18.1`):

```
cargo xtask i18n extract
cargo xtask i18n pseudo
```

and commit the results. CI runs `cargo xtask i18n check`, which fails when
they are out of date. To bring a language up to date with a new template:

```
msgmerge -U translations/vi/LC_MESSAGES/taurus-desktop.po translations/taurus-desktop.pot
```
