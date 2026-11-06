# Hyades (codename Taurus)

Hyades (codename Taurus) is an open-source, cross-platform desktop download manager written in Rust, with a segmented HTTP engine, resumable downloads, torrents, a command-line client and a native desktop app (alpha). Status: v2 rewrite in progress; see the [design spec](docs/superpowers/specs/2026-10-27-taurus-v2-design.md).

## Usage

The command-line client, `hyades-cli`, drives the Hyades (codename Taurus) engine. It is a queue runner: `add`, `resume`, `run` and `start-now` queue their downloads, then run the whole queue in the foreground until nothing is queued, waiting or downloading (and until a `when_done` countdown fires or is cancelled). At most `max_active` downloads run at once; a download whose connection drops waits and retries on its own (after 1 minute, doubling up to 10 minutes). Ctrl-C stops the running downloads, keeping their progress, and leaves them queued with the rest; `hyades-cli run` (or any other queue command) continues them. During a `when_done` countdown Ctrl-C cancels the countdown instead. Downloads interrupted by a crash, a kill or a reboot come back queued the same way and continue with the next queue run; `hyades-cli list` shows them as Queued. Before an interrupted download goes on, its link is checked again, and a file that changed on the server is downloaded again from the start. Free space is checked when a download is added and again when it starts, counting what the other running downloads to the same disk still need; one that no longer fits fails with `DiskFull` (free some space, then `hyades-cli resume` it). Exit codes: 0 every download of the run completed, 1 a download failed or an error, 2 stopped by Ctrl-C or a download was paused.

```sh
# Download into a folder (default: your Downloads folder), with up to 8 connections
hyades-cli add https://example.com/file.iso --dir ~/Downloads --connections 8

# Several downloads at once: URLs on the command line and in a file
# (one URL per line; blank lines and lines starting with # are skipped)
hyades-cli add https://example.com/a.zip https://example.com/b.zip --file urls.txt

# One file from several servers, checked against its SHA-256 once downloaded
hyades-cli add https://example.com/file.iso --mirror https://mirror.example.org/file.iso \
  --checksum e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855 --name file.iso

# Send a Referer with every request (some hosts require the page that links the file)
hyades-cli add https://cdn.example.com/video.mp4 --referer https://example.com/watch

# Continue paused or failed downloads 1 and 3
hyades-cli resume 1 3

# Run whatever is queued or waiting, including downloads stopped by Ctrl-C,
# a crash or a reboot
hyades-cli run

# Start download 4 now, even if max_active downloads are already running
hyades-cli start-now 4

# Show every download: id, state, kind (http, torrent or media), percent, size, name
hyades-cli list

# Forget download 1 and delete its partial or finished file
hyades-cli remove 1 --delete-files
```

### Links on a web page

`hyades-cli grab <PAGE_URL>` reads a web page (or an Apache/nginx style "Index of" folder listing) and lists the files it links to at once: index (from 0), the folder it was found in, and name. Only links whose file name has an extension that is not a web page's are listed, each once, at most 500. Sizes are not asked for unless you add `--sizes`: after the listing, one line per file (its index and size, `-` when the server tells none) is printed as each answer arrives, so the lines come in the order the servers answer, not the listing's.

```sh
# List the files a page links to
hyades-cli grab https://example.com/papers/

# Only some extensions (comma-separated, any case)
hyades-cli grab https://example.com/papers/ --ext pdf,zip

# A folder listing: read its subfolders too (one level)
hyades-cli grab https://example.com/files/ --subfolders

# Also ask for each file's size (printed by index as the answers arrive)
hyades-cli grab https://example.com/papers/ --sizes

# Add every listed file to the queue and download them into a folder; a file found in
# a subfolder goes into the same subfolder of that folder (a/x.pdf and b/x.pdf stay apart)
hyades-cli grab https://example.com/papers/ --ext pdf --add --dir ~/Downloads
```

`--json` prints the listing as one object, `{"links": [{"index", "url", "name", "folder", "extension"}], "truncated": bool}`; with `--sizes` it is followed by one object per size as it arrives, `{"index", "size"}` (`size` is `null` when the server tells none). Without `--add`, nothing is queued; with `--add`, `--json` goes on to print the usual event lines after that object, one JSON value per line. `--add` never waits on sizes, and does not go with `--sizes`. Folder names come from the page, so they are checked: a name that is empty, `.`, `..` or holds a backslash makes that one file fail (the others go on, exit 1), and nothing is ever written outside `--dir`. With `--add`, a file on the page's own scheme, host and port is requested with the page (without its `#fragment`) as its Referer, like a browser following the link; a plain-http link on an https page gets none.

### Protected downloads

A web download that asks for a user name and password (HTTP Basic sign-in) takes `--user`; the password is asked for on the terminal, without echo, and nothing else:

```sh
# Asks "Password for alice:" on the terminal
hyades-cli add https://files.example.com/report.zip --user alice

# Give a download that failed with NeedsSignIn (or was paused) its user name and
# password; it then resumes and the queue runs until it is idle
hyades-cli login 3 --user alice
```

- The user name cannot contain `:` (HTTP Basic sign-in has no room for it; `--user alice:secret` is refused, and nothing is stored). The password is asked for before anything else starts, so `hyades-cli` holds no lock on its data while you type; if another `hyades-cli` (or the desktop app) is using the data, that is said first, before you type anything. The prompt goes to stderr, so `--json` output stays clean.
- `--user` takes exactly one URL (not several, nor `--file`): one password is sent to one link's host.
- The password is never an option and is never read from the environment or a pipe: with no terminal, `hyades-cli` exits 1 and says so.
- It goes to the operating system's keychain (Keychain on macOS, Credential Manager on Windows, the Secret Service on Linux), filed under the service `dev.nikethai.taurus`. It is never written to the job database, the settings, the logs or the output, and it is sent only to the link's own host (mirrors on that host included).
- On macOS the keychain may ask once for permission when the `hyades-cli` command reads a password the desktop app saved, or the other way round; choose "Always Allow" so it does not ask again.
- When there is no keychain (for example a Linux session without a Secret Service), the password is kept in memory only, until `hyades-cli` exits. A download that needs it after a restart then fails with `NeedsSignIn: the server asks for a user name and password`: run `hyades-cli login <ID> --user <NAME>` and type the password again.
- Only web downloads can be protected, not torrents or video links. A proxy with a user name or password is not supported either (see Settings).

### Torrents

`add` also takes magnet links, `.torrent` links and `.torrent` files, mixed freely with web links; `list` shows each download's kind (`http` or `torrent`), and a torrent row shows `peers N` while it downloads and `sharing · ratio 0.42` while it shares. A torrent is saved in a folder of its own name inside `--dir`; a second torrent with the same name gets `Name (1)`. A magnet link whose peers cannot be found within two minutes waits (state Waiting) and is retried. At most `torrent.max_active` torrents download at once, and sharing ones do not count.

When a torrent finishes it keeps sharing (uploading to other peers) until its ratio, uploaded over downloaded size, reaches `torrent.ratio` (default 1.0), or for `torrent.seed_time` if set. Hyades shares only while it is running downloads; the CLI exits when downloads finish (`add` and `run` return once nothing is downloading, queued or waiting, even if torrents are still sharing) and says so with `Sharing pauses when Hyades exits; it continues the next time Hyades runs (up to ratio 1.0).` A torrent that has not reached its limit shares again whenever Hyades next runs downloads. Ctrl-C stops sharing too.

```sh
# A magnet link, a .torrent file and a .torrent link
hyades-cli add "magnet:?xt=urn:btih:c12fe1c06bba254a9dc9f519b335aa7c1367a88a&dn=ubuntu" --dir ~/Downloads
hyades-cli add ./ubuntu.torrent https://example.com/other.torrent

# See a torrent's files (index, size, path) without adding it; --json prints them as JSON.
# For a magnet link this first fetches the file list from its peers.
hyades-cli torrent-files ./ubuntu.torrent

# Download only files 0, 2 and 5 to 7 (one torrent only; a range includes both ends)
hyades-cli add ./ubuntu.torrent --files 0,2,5-7

```

When a torrent's file selection changes after it was added (even while it downloads or shares), files that stay selected keep what is downloaded, and newly selected files must fit on the disk. Files you deselect keep what they already downloaded on disk; delete them yourself if you no longer want them. A torrent that had finished and gains files checks its files again first, which may update the kept files' modification time (not their content).

```sh
# Change the files of a torrent that is already added (id from `hyades-cli list`):
# the same list syntax as --files; at least one file, each index within the torrent
hyades-cli torrent-select 4 0,2,5-7

# Give one torrent its own sharing rules; `default` goes back to the global setting
hyades-cli job set 4 ratio 2.5            # a number, never, or default
hyades-cli job set 4 seed_time 12h        # 30m, 2h, 1d, off, or default
hyades-cli job set 4 ratio default
```

`torrent-select` says so when the torrent is queued again (run `hyades-cli run` to download the new files) and fails, changing nothing, when the files do not fit on the disk. `job set` keeps the torrent's other rule as it was.

The torrent settings are changed like the others: `hyades-cli config set torrent.ratio 2`.

| Key | Values | Default |
| --- | --- | --- |
| `torrent.ratio` | stop sharing at this uploaded/size ratio of the files you chose: a number such as `1.5`, `0` (stop as soon as the download finishes), or `never` | `1.0` |
| `torrent.seed_time` | also stop sharing this long after the download finished: `30m`, `2h`, `1d` (or `90s`), or `off` | `off` |
| `torrent.upload_limit` | upload speed cap: `off`, `500KB/s` or `2MB/s` | `off` |
| `torrent.download_limit` | cap on the torrents' share of the download speed, same values | `off` |
| `torrent.max_active` | how many torrents download at once, at least 1 | `3` |
| `torrent.peers` | the most peers per torrent: a number, or `default` | `default` |
| `torrent.port` | the port torrents listen on: `1024` to `65535`, or `random` | `random` |
| `torrent.upnp` | `on` or `off`: ask the router to open the port | `on` |
| `torrent.dht` | `on` or `off`: find peers through the distributed hash table | `on` |

`torrent.peers`, `torrent.port`, `torrent.upnp` and `torrent.dht` apply the next time Hyades starts. The others take effect at once, also for torrents that are running. The overall `speed` is shared evenly between web downloads and torrents while both run. A torrent's pieces are always downloaded in order.

`--name`, `--mirror` (repeatable) and `--checksum` apply to a single web download, so they need exactly one URL; `--files` needs exactly one torrent; `--dir`, `--connections` and `--referer` apply to every URL of the `add`.

### Video sites

`add` also takes links to video and audio sites (YouTube, Vimeo, Twitch and many more, read by [yt-dlp](https://github.com/yt-dlp/yt-dlp)), a playlist, or a live stream. The first time, Hyades downloads the tools it needs (see below), which takes a minute. `list` shows these as kind `media`; a playlist row shows `2 of 5 videos`, and a live stream shows `live 2m` while it records. A playlist is saved in a folder of its own inside `--dir`.

```sh
# The best video and sound, as one MP4
hyades-cli add https://www.youtube.com/watch?v=VIDEO_ID

# Sound only: best audio, or a small MP3 with --quality small
hyades-cli add https://www.youtube.com/watch?v=VIDEO_ID --audio
hyades-cli add https://www.youtube.com/watch?v=VIDEO_ID --audio --quality small

# At most 720p: best, or any height the video has, such as 1080p, 720p or 480p (see
# hyades-cli inspect); a quality the video lacks falls back to best, with a note.
# --quality audio is the same as --audio
hyades-cli add https://vimeo.com/VIDEO_ID --quality 720p

# What a link offers, without adding it: the title, every quality, the subtitle
# languages and the captions the site generates (--json prints it as one object)
hyades-cli inspect https://www.youtube.com/watch?v=VIDEO_ID
hyades-cli --json inspect https://www.youtube.com/watch?v=VIDEO_ID --cookies-from firefox

# Convert the result: mp3, or smaller-mp4
hyades-cli add https://www.youtube.com/watch?v=VIDEO_ID --convert smaller-mp4

# A playlist of more than 50 videos needs --yes
hyades-cli add "https://www.youtube.com/playlist?list=PLAYLIST_ID" --yes

# A live stream records until it ends; stop it from another terminal and keep what was recorded
hyades-cli add https://www.twitch.tv/CHANNEL
hyades-cli stop 5
```

`hyades-cli stop <ID>` stops a running video download or recording from another terminal, keeping what was downloaded (a live stream is made playable), and says how it ended. It asks the `hyades-cli` process or the desktop app that runs the job, which answers within a moment; if none answers within 5 seconds it exits 1. Ctrl-C in the process that runs it also stops the job, but leaves it queued, not finished.

Video links also take these options:

```sh
# Sign in with a browser's cookies, for videos that need a login or a "not a bot" check
# (chrome, firefox, safari, edge, brave, chromium, opera or vivaldi, optionally :PROFILE)
hyades-cli add https://www.youtube.com/watch?v=VIDEO_ID --cookies-from firefox
hyades-cli add https://www.youtube.com/watch?v=VIDEO_ID --cookies-from chrome:"Profile 1"

# Retry a video that failed for want of a sign-in, with those cookies
hyades-cli resume 5 --cookies-from firefox

# Subtitles: some languages (or all), with --auto-subs the captions the site generates,
# with --embed-subs also inside the video file (not for WebM or sound)
hyades-cli add https://www.youtube.com/watch?v=VIDEO_ID --subs en,vi --auto-subs --embed-subs

# The file format of the video: mp4 (the default), mkv or webm
hyades-cli add https://www.youtube.com/watch?v=VIDEO_ID --container mkv
```

`--auto-subs` and `--embed-subs` need `--subs`, and `--convert smaller-mp4` makes an MP4, so it cannot be used with `--container mkv` or `webm`. yt-dlp reads the browser's cookies itself; only the browser's name is stored with the download.

Convert a finished download, or any file that is not being downloaded, with ffmpeg:

```sh
# A finished download (a playlist makes one conversion per file), or a file by path
hyades-cli convert 5 mp3
hyades-cli convert ~/Music/talk.webm smaller-mp4
```

The result is a new file next to the original, `<name> (mp3).mp3` or `<name> (smaller).mp4` (a name that is taken gets ` (1)`, ` (2)`, ...); the original is never changed or overwritten. The conversion is a download of kind `convert` in the queue, and `convert` runs the queue until it is idle.

#### The tools

Video downloads use yt-dlp, ffmpeg and Deno. Hyades downloads them itself, checks each against its published SHA-256 and keeps them in `<data dir>/tools/<tool>/<version>/`; the file `<data dir>/tools/<tool>/current` names the version in use, and the previous version is kept for a rollback. They are not installed system-wide and the ones on your `PATH` are not used. yt-dlp is checked for a newer version once a day while videos download (sites change often), ffmpeg and Deno once a week.

```sh
hyades-cli tools                    # version, state and path of each tool (--json: a JSON array; works while a download runs)
hyades-cli tools install            # download what is missing, ahead of the first video
hyades-cli tools update --force     # look for newer versions now
hyades-cli tools rollback yt-dlp    # back to the previous version (also: ffmpeg, deno)
```

With `--json`, `install` and `update` print each tool event as a JSON line, then the final array; a tool's `state` is `"Missing"`, `"Ready"`, `{"Installing":{...}}` or `{"Failed":"<kind>"}`.

Licenses: yt-dlp is released under the Unlicense, the ffmpeg builds (from [yt-dlp/FFmpeg-Builds](https://github.com/yt-dlp/FFmpeg-Builds), and [ffmpeg.martin-riedl.de](https://ffmpeg.martin-riedl.de) on macOS) are GPL builds, and Deno is MIT. They are separate programs that Hyades starts; none is linked into it.

### Settings

`hyades-cli config get` prints every setting as `key = value`; `hyades-cli config get <KEY>` prints one. `hyades-cli config set <KEY> <VALUE>` changes one:

| Key | Values | Default |
| --- | --- | --- |
| `max_active` | how many downloads run at once, at least 1 | `3` |
| `speed` | `fastest` (no limit), `balanced` (60 % of the fastest speed measured), `gentle` (25 %), or a fixed limit such as `500KB/s` or `5MB/s` (KB = 1000 bytes, MB = 1000000 bytes) | `fastest` |
| `when_done` | what to do once the queue is done (a download finishing empties it; pausing or removing the last one does not count): `nothing`, `notify` (prints `All downloads finished.`), `sleep` or `shutdown` (after a 60 second countdown; one-shot: once it runs, `when_done` goes back to `nothing`), `quit` | `nothing` |
| `auto_checksum` | `on` or `off`: verify a download against a `SHA256SUMS` file the host publishes beside it | `on` |
| `keep_awake` | `on` or `off`: keep the computer from sleeping while a download runs | `on` |
| `network.proxy` | how to reach the web: `system` (the OS settings; on Linux the `HTTP_PROXY`, `HTTPS_PROXY` and `NO_PROXY` variables), `none` (direct), or a proxy URL: `http://`, `https://`, `socks5://` or `socks5h://` (names resolved by the proxy) | `system` |

```sh
hyades-cli config set max_active 2
hyades-cli config set speed 5MB/s
hyades-cli config set when_done shutdown
```

```sh
hyades-cli config set network.proxy socks5h://127.0.0.1:9050
hyades-cli config set network.proxy none
```

The proxy is used by downloads, yt-dlp and tool downloads from the next request on, and by torrents (a SOCKS proxy only) the next time Hyades starts; setting or leaving a SOCKS proxy prints `applies next time Hyades starts`. A proxy URL with a user name or password is refused (they would be stored in the settings), as is anything that is not one of the schemes above; `config set` then exits 1 and shows the reason.

Global options go before the command:

- `--json` prints every event as one JSON line on stdout; `list --json` prints one JSON array and `config get --json` one JSON object.
- `--data-dir <PATH>` sets where the job database lives. Without it, `HYADES_DATA_DIR` is used, else the OS data directory. `TAURUS_DATA_DIR` from earlier builds is still read.
- `--dry-run-power` prints the sleep or shutdown command of `when_done` (`would run: ...`) instead of running it.

## Desktop app (alpha)

Hyades also has a desktop app with a simple window: paste a link, see what it is, download it. It handles web downloads, torrents (magnet and `.torrent` links), and videos, audio and live streams from video sites, on Windows, macOS and Linux.

```sh
cargo run -p taurus-desktop --bin hyades
```

- **Data.** The app keeps its downloads list and settings in the same folder as the command-line client: `HYADES_DATA_DIR` if set, else `~/.local/share/taurus` on Linux (`$XDG_DATA_HOME/taurus` when that is set), `~/Library/Application Support/dev.nikethai.taurus` on macOS and `%APPDATA%\nikethai\taurus\data` on Windows. Only one of them can use the folder at a time; opening the app while the command-line client runs shows a message and exits.
- **Tray.** Closing the window hides it to the tray and downloads go on; choose Quit in the tray menu to stop. On Linux the tray needs a StatusNotifier host (KDE Plasma has one; GNOME needs the AppIndicator extension). Without a tray, closing the window quits.
- **One window.** Launching the app again (for example `hyades <link>`, or with a `.torrent` file) shows the open window, from the tray or minimized, and hands it the links; other arguments are ignored.

### Advanced mode

Turn on **Show advanced options** in Settings (turn it off again in Advanced settings, General) for a denser window with filters, details and every option. The screens, with no screenshots yet:

- **Compact list** (window narrower than 900 px). The downloads under three tabs with counts: Active (downloading, recording, sharing), Waiting (queued, waiting, paused, failed) and Done. Each row shows the kind, the name with its source (HTTP, Torrent, yt-dlp, Convert), progress, status and speed, and the action that fits (Pause, Resume, Try again, Stop, Open). "+" opens a link field with "Several links…" and "Grab links from a page…"; the gear opens Settings. The footer shows the total download and upload speed and the when-done button.
- **Wide list** (900 px and wider). A sidebar of filters with counts, Library (All, Downloading, Queued, Paused, Completed, Failed) and Sources (HTTP, Torrents, Media), a search field over the list, and the selected download's details in a pane on the right. Stretching the window across 900 px switches between the two layouts and keeps the selection.
- **Details** (click a row; over the whole window when compact, Back closes it). The size and percent, a segment map that fills in as each connection downloads its part, the steps of a video (tools, download, finish), figures such as speed, connections, peers, ratio or mirrors, where it is saved and its link, and a torrent's files with their progress. Actions: Pause or Resume, Open, Show in folder, Copy link, Convert… (to MP3, or a smaller MP4, for a finished video), Choose files… and the sharing limits of a torrent, and Remove….
- **Remove.** Asks first. A finished download's files stay unless "Also delete the files" is ticked; an unfinished download's partial files are always cleared.
- **Sign in.** A download that wants a user name and password (Enter password… in its details, or on its own when the details open) asks for them in a dialog, says when they did not work, and resumes once they do. The password is kept in the computer's keychain, never in the settings.
- **Browser sign-in retry.** A video that failed for want of a sign-in or a "not a bot" check offers to try again with the cookies of Chrome, Firefox, Edge, Brave or (on macOS) Safari.
- **Advanced add** (paste a link). What the link is, then the options of its kind. Files: file name, connections per file, mirrors (one link per line), SHA-256 checksum, Referer, user name and password. Videos: quality, format (MP4, MKV or WebM), subtitle languages with the site's generated captions and embedding, and the browser to sign in with. Torrents: the file picker and how long to keep sharing. Then where it is saved and "Start immediately". A link to a web page offers "Find files →".
- **Torrent file picker.** The files grouped by top folder, with a box per folder (all, some or none), All and None, the selected count and size and the folder's free space. It is part of Advanced add, and "Choose files…" in a torrent's details opens it again to add or drop files later.
- **Grab links** ("+", then "Grab links from a page…", or "Find files →"). Reads a web page or an "Index of" folder listing and lists the files it links to, with type chips (All, Video, Docs, Archives, Images) and counts, an extension filter, a Subfolders switch (one level into a listing's folders), "Select all shown", sizes as they arrive, and "Download N". Files from subfolders keep their subfolder.
- **Several links** ("+", then "Several links…"). One link per line (lines starting with # are skipped); each line is shown with its kind. "Treat copies as mirrors" makes links to files with the same name one download from several servers; a web page line offers "Find files →". "Add N downloads" adds them.
- **When done and speed** (the footer's when-done button). What to do when downloads finish (do nothing, notify, sleep, shut down or quit Hyades; sleep and shutdown give 60 seconds to cancel) and the speed: Gentle, Balanced, Fastest or Custom. A custom speed is typed in MB/s with a dot for decimals (2.5; a comma is refused) and applies on Enter or when the field loses focus.
- **Advanced settings** (the gear). One section after another when compact, a list of sections beside them when wide: Downloads (folder, sorting by type, downloads at once, connections per file, speed, checksums, keeping the computer awake), Torrents (sharing ratio and time, upload and download limits, torrents at once, peers, listening port, UPnP, DHT), Network (proxy: system, none or manual), Engines (yt-dlp, ffmpeg and Deno: install, check for newer versions, roll back), Appearance (theme, accent, progress bars colored by file type), Privacy ("Forget saved passwords" removes every download password from the keychain) and General (copied-link offers, open at login, Show advanced options). A typed value applies on Enter or when its field loses focus.

Keyboard: Tab moves through the controls in order (in the list: the header, the tabs or filters, each row then its action button, the footer), Space or Enter presses the focused one. In a dialog, Enter confirms, Escape cancels and Tab stays inside it. Escape also closes the when-done menu and the "+" link field, and goes back from the details (compact), Advanced add, Grab links, Several links and Settings.

Known alpha limitations: dragging links or files in from other apps does not work yet (the UI toolkit does not support it), and the colours and icons are provisional.
