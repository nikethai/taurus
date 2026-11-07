# Changelog

All notable changes to Hyades are listed in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [1.0.0] - Unreleased

The first release of Hyades, a rewrite of the 2022 prototype from scratch.
Everything below is new. The headings follow the milestones it was built in.

### M0–M1: Web downloads

- Web (HTTP and HTTPS) downloads in parallel parts over several connections. A connection that finishes its part takes over the rest of a slower one.
- Downloads survive a crash, a kill or a reboot and continue exactly where they stopped. Progress is saved safely as it goes, and an unfinished file ends in `.tapa` until it is complete.
- Safe file names, taken from the server or the link.
- The command-line client, `hyades-cli`: `add`, `resume`, `list` and `remove`, with `--json` output for scripts.

### M2: Queue, speed limits and errors

- A download queue: at most `max_active` downloads run at once, `run` continues whatever is queued, and `start-now` starts one at once.
- Speed presets (Fastest, Balanced, Gentle) and fixed speed limits.
- Mirrors (one file from several servers), a Referer for hosts that need one, and SHA-256 checks, given by hand or read from a `SHA256SUMS` file the host publishes.
- Hyades learns how many connections each server allows.
- Plain-language errors. A link that gives a web page, a bot check or a "quota exceeded" page instead of the file is recognized and said so.
- Link fixes for Dropbox, GitHub and Google Drive, including Google Drive's "download anyway" page.
- Free space is checked when a download is added and again when it starts, and files too big for a FAT32 disk are caught early.
- Finished files get the operating system's "downloaded from the internet" mark.
- A download whose connection drops waits and tries again on its own (after 1 minute, doubling up to 10 minutes). Interrupted downloads come back queued, an expired link is resolved again from the link as added, and a file that changed on the server starts over.
- When-done actions: notify, sleep, shut down or quit (sleep and shut down wait 60 seconds, so you can cancel), and keeping the computer awake while downloading.

### M3: Torrents

- Magnet links, `.torrent` files and `.torrent` links, in the same queue as web downloads.
- Choose which files of a torrent to download (`hyades-cli torrent-files` lists them).
- Sharing rules: stop sharing at a ratio, after a time, or never.
- Torrent settings: upload and download limits, torrents at once, peers per torrent, listening port, UPnP and DHT.
- Torrents continue after a restart without downloading again what they already have.

### M4: Desktop app, Simple mode

- The desktop app, `hyades`, in Simple mode: a welcome screen, the downloads under Downloading and Finished, paste a link and see what it is before downloading it, and short settings (save folder and sorting by type, speed, when done, copied-link offers, open at login).
- Light, dark and system themes, with six accent colors.
- A tray icon (closing the window keeps downloads going), notifications, an offer to download links you copy, and open at login.
- One window: launching Hyades again, with a link or a `.torrent` file, hands it to the window that is already open.

### M5: Videos, audio and live streams

- Video and audio from the sites yt-dlp supports: best quality or a chosen height, sound only, playlists (saved in a folder of their own) and live streams, recorded until they end or you stop them.
- Converting to MP3 or a smaller MP4.
- yt-dlp, ffmpeg and Deno are downloaded on first use, checked against their published SHA-256, kept up to date (yt-dlp daily, the others weekly) and can be rolled back (`hyades-cli tools`). They are never installed system-wide.
- Videos work from Simple mode and the command line, and `hyades-cli stop` stops a recording from another terminal, keeping what was recorded.

### M6a: More options in the engine

- Grab links from a web page or an "Index of" folder listing, with extension filters, one level of subfolders and file sizes (`hyades-cli grab`).
- Downloads that ask for a user name and password. The password is kept in the operating system's keychain, never in the settings (`hyades-cli login`).
- A manual proxy (HTTP, HTTPS or SOCKS5) besides the system proxy.
- For videos: sign in with a browser's cookies, subtitles (chosen languages, the site's generated captions, embedded in the file), the file format (MP4, MKV or WebM), and `hyades-cli inspect` to see what a link offers.
- Convert any finished download or file with ffmpeg (`hyades-cli convert`). The original is never changed.
- Change a torrent's files after it was added, and give one torrent its own sharing rules (`hyades-cli torrent-select`, `hyades-cli job set`).

### M6b: Advanced mode

- Advanced mode, turned on with "Show advanced options": a compact list with Active, Waiting and Done tabs, and a wide layout from 900 px with filters, search and a details pane.
- Details for each download: a segment map, the steps of a video, speed, connections, peers, ratio and mirrors, a torrent's files, and actions (Open, Show in folder, Copy link, Convert…, Choose files…, Remove…).
- Advanced add with every option for files, videos and torrents, including a torrent file picker.
- Grab links from a page, add several links at once (with copies treated as mirrors), and a when-done and speed menu in the footer.
- Full settings: downloads, torrents, network, engines, appearance, privacy and general.
- Sign-in and "try again with browser cookies" dialogs, and full keyboard use.

### M7: Installers, updates and translations

- Installers for every platform: Windows (x64 and arm64), one macOS build for Apple silicon and Intel, and an AppImage, a deb and an rpm for Linux (x64 and arm64). The command-line client comes as separate archives.
- The product is named Hyades everywhere: the programs are `hyades` and `hyades-cli`, and `HYADES_DATA_DIR` moves the data folder (`TAURUS_DATA_DIR` from earlier builds is still read).
- In-app updates: Hyades checks 30 seconds after launch and then every 24 hours. Windows, macOS and the AppImage download the update in the background and install it when you click Restart; deb and rpm installs show a notice with a link to the new package. The check can be turned off in settings.
- Translations: the app's text is ready for translation (see `TRANSLATING.md`), `HYADES_LANG` picks the language, and `HYADES_LANG=en_XA` shows a pseudo-locale for testing.
- On Linux, Hyades is offered as a handler for magnet links and `.torrent` files, without taking over an existing default.
- Sign in from Simple mode, a user name and password for Several links, the reason a tool failed to install, and a when-done menu that closes after a pick.
- Unselected torrent files no longer leave empty files behind, and passwords are removed from every link written to the logs.
- A visual pass against the design, the Inter typeface built in, layouts that hold with long translations, and labels for screen readers on every control.

### Known issues

- Selecting a torrent file again after its empty placeholder file was deleted makes Hyades check the whole torrent again. On a large torrent this takes a while, and it updates the modification time (not the content) of files that were already complete.

[1.0.0]: https://github.com/nikethai/taurus/releases/tag/v1.0.0
