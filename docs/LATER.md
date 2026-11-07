# Later

Things that are not in Hyades 1.0, written down so they are not lost. Nothing
here is promised for a particular release unless it says so.

The next milestone after 1.0 is the browser extension for Chromium and Firefox
(M8, for 1.x): it hands links to Hyades with the browser's cookies, Referer and
exact User-Agent, which is the answer to downloads behind Cloudflare checks
or a login.

## Link and file associations on Windows and macOS (for 1.1)

On Linux, Hyades is offered as a handler for magnet links and `.torrent`
files and never takes over an existing default. Windows and macOS have no
associations in 1.0:

- **Windows.** The installer template (cargo-packager's NSIS) always writes
  `HKCU\Software\Classes\magnet`, which would silently take magnet links away
  from qBittorrent and similar apps. Hyades needs an association that offers
  itself without taking over.
- **macOS.** macOS hands opened links to an app as Apple Events, which the UI
  toolkit (Slint on winit 0.30) does not pass on. A declared handler would
  launch Hyades and drop the link.

Until then, the offer to download a copied link and pasting a link cover
both, and the browser extension will add more.

## Parked during the 1.0 work

- Proxy user names and passwords, kept in the keychain. Today a proxy URL
  with a user name or password is refused.
- Treating a quality such as `0480p` as `480p`.
- `hyades-cli inspect` lists the audio choices `audio-best` and
  `audio-small`, which `--quality` does not accept (it takes `audio` and
  `small`).
- An empty temporary folder that a crash at the wrong moment can leave
  behind.
- Checking yt-dlp's `SHA2-256SUMS.sig` signature, on top of the SHA-256
  checks Hyades already does.
- Colors in `hyades-cli` on the legacy Windows console.
- A language picker in the window. With English as the only language,
  `HYADES_LANG` and the `ui.language` setting are enough until the first real
  translation lands.

## From the design's "Later" list

- Vietnamese translation.
- A scheduler (start or stop downloads at set times).
- Remote control.
- A portable Windows build.
- ytarchive as a second engine for live streams.
- FTP downloads.
- Deeper page crawling for grab links (today: one level, plus folder
  listings).
- Holding large downloads on metered connections.
