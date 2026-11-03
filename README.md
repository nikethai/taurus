# Hyades (codename Taurus)

Hyades (codename Taurus) is an open-source, cross-platform desktop download manager written in Rust, with a segmented HTTP engine, resumable downloads, torrents, a command-line client and a native desktop app (alpha). Status: v2 rewrite in progress; see the [design spec](docs/superpowers/specs/2026-10-27-taurus-v2-design.md).

## Usage

The command-line client, `taurus`, drives the Hyades (codename Taurus) engine. It is a queue runner: `add`, `resume`, `run` and `start-now` queue their downloads, then run the whole queue in the foreground until nothing is queued, waiting or downloading (and until a `when_done` countdown fires or is cancelled). At most `max_active` downloads run at once; a download whose connection drops waits and retries on its own (after 1 minute, doubling up to 10 minutes). Ctrl-C stops the running downloads, keeping their progress, and leaves them queued with the rest; `taurus run` (or any other queue command) continues them. During a `when_done` countdown Ctrl-C cancels the countdown instead. Downloads interrupted by a crash, a kill or a reboot come back queued the same way and continue with the next queue run; `taurus list` shows them as Queued. Before an interrupted download goes on, its link is checked again, and a file that changed on the server is downloaded again from the start. Free space is checked when a download is added and again when it starts, counting what the other running downloads to the same disk still need; one that no longer fits fails with `DiskFull` (free some space, then `taurus resume` it). Exit codes: 0 every download of the run completed, 1 a download failed or an error, 2 stopped by Ctrl-C or a download was paused.

```sh
# Download into a folder (default: your Downloads folder), with up to 8 connections
taurus add https://example.com/file.iso --dir ~/Downloads --connections 8

# Several downloads at once: URLs on the command line and in a file
# (one URL per line; blank lines and lines starting with # are skipped)
taurus add https://example.com/a.zip https://example.com/b.zip --file urls.txt

# One file from several servers, checked against its SHA-256 once downloaded
taurus add https://example.com/file.iso --mirror https://mirror.example.org/file.iso \
  --checksum e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855 --name file.iso

# Send a Referer with every request (some hosts require the page that links the file)
taurus add https://cdn.example.com/video.mp4 --referer https://example.com/watch

# Continue paused or failed downloads 1 and 3
taurus resume 1 3

# Run whatever is queued or waiting, including downloads stopped by Ctrl-C,
# a crash or a reboot
taurus run

# Start download 4 now, even if max_active downloads are already running
taurus start-now 4

# Show every download: id, state, kind (http, torrent or media), percent, size, name
taurus list

# Forget download 1 and delete its partial or finished file
taurus remove 1 --delete-files
```

### Links on a web page

`taurus grab <PAGE_URL>` reads a web page (or an Apache/nginx style "Index of" folder listing) and lists the files it links to: index (from 0), size (`-` when the server tells none), the folder it was found in, and name. Only links whose file name has an extension that is not a web page's are listed, each once, at most 500.

```sh
# List the files a page links to
taurus grab https://example.com/papers/

# Only some extensions (comma-separated, any case)
taurus grab https://example.com/papers/ --ext pdf,zip

# A folder listing: read its subfolders too (one level)
taurus grab https://example.com/files/ --subfolders

# Add every listed file to the queue and download them into a folder; a file found in
# a subfolder goes into the same subfolder of that folder (a/x.pdf and b/x.pdf stay apart)
taurus grab https://example.com/papers/ --ext pdf --add --dir ~/Downloads
```

`--json` prints the listing as one object, `{"links": [{"index", "url", "name", "folder", "extension", "size"}], "truncated": bool}`. Without `--add`, nothing is queued; with `--add`, `--json` goes on to print the usual event lines after that object, one JSON value per line. Folder names come from the page, so they are checked: a name that is empty, `.`, `..` or holds a backslash makes that one file fail (the others go on, exit 1), and nothing is ever written outside `--dir`. With `--add`, a file on the page's own scheme, host and port is requested with the page (without its `#fragment`) as its Referer, like a browser following the link; a plain-http link on an https page gets none.

### Protected downloads

A web download that asks for a user name and password (HTTP Basic sign-in) takes `--user`; the password is asked for on the terminal, without echo, and nothing else:

```sh
# Asks "Password for alice:" on the terminal
taurus add https://files.example.com/report.zip --user alice

# Give a download that failed with NeedsSignIn (or was paused) its user name and
# password; it then resumes and the queue runs until it is idle
taurus login 3 --user alice
```

- The user name cannot contain `:` (HTTP Basic sign-in has no room for it; `--user alice:secret` is refused, and nothing is stored). The password is asked for before anything else starts, so `taurus` holds no lock on its data while you type; if another `taurus` (or the desktop app) is using the data, that is said first, before you type anything. The prompt goes to stderr, so `--json` output stays clean.
- `--user` takes exactly one URL (not several, nor `--file`): one password is sent to one link's host.
- The password is never an option and is never read from the environment or a pipe: with no terminal, `taurus` exits 1 and says so.
- It goes to the operating system's keychain (Keychain on macOS, Credential Manager on Windows, the Secret Service on Linux), filed under the service `dev.nikethai.taurus`. It is never written to the job database, the settings, the logs or the output, and it is sent only to the link's own host (mirrors on that host included).
- On macOS the keychain may ask once for permission when the `taurus` command reads a password the desktop app saved, or the other way round; choose "Always Allow" so it does not ask again.
- When there is no keychain (for example a Linux session without a Secret Service), the password is kept in memory only, until `taurus` exits. A download that needs it after a restart then fails with `NeedsSignIn: the server asks for a user name and password`: run `taurus login <ID> --user <NAME>` and type the password again.
- Only web downloads can be protected, not torrents or video links. A proxy with a user name or password is not supported either (see Settings).

### Torrents

`add` also takes magnet links, `.torrent` links and `.torrent` files, mixed freely with web links; `list` shows each download's kind (`http` or `torrent`), and a torrent row shows `peers N` while it downloads and `sharing · ratio 0.42` while it shares. A torrent is saved in a folder of its own name inside `--dir`; a second torrent with the same name gets `Name (1)`. A magnet link whose peers cannot be found within two minutes waits (state Waiting) and is retried. At most `torrent.max_active` torrents download at once, and sharing ones do not count.

When a torrent finishes it keeps sharing (uploading to other peers) until its ratio, uploaded over downloaded size, reaches `torrent.ratio` (default 1.0), or for `torrent.seed_time` if set. Taurus shares only while it is running downloads; the CLI exits when downloads finish (`add` and `run` return once nothing is downloading, queued or waiting, even if torrents are still sharing) and says so with `Sharing pauses when Taurus exits; it continues the next time Taurus runs (up to ratio 1.0).` A torrent that has not reached its limit shares again whenever Taurus next runs downloads. Ctrl-C stops sharing too.

```sh
# A magnet link, a .torrent file and a .torrent link
taurus add "magnet:?xt=urn:btih:c12fe1c06bba254a9dc9f519b335aa7c1367a88a&dn=ubuntu" --dir ~/Downloads
taurus add ./ubuntu.torrent https://example.com/other.torrent

# See a torrent's files (index, size, path) without adding it; --json prints them as JSON.
# For a magnet link this first fetches the file list from its peers.
taurus torrent-files ./ubuntu.torrent

# Download only files 0, 2 and 5 to 7 (one torrent only; a range includes both ends)
taurus add ./ubuntu.torrent --files 0,2,5-7

```

When a torrent's file selection changes after it was added (even while it downloads or shares), files that stay selected keep what is downloaded, and newly selected files must fit on the disk. Files you deselect keep what they already downloaded on disk; delete them yourself if you no longer want them. A torrent that had finished and gains files checks its files again first, which may update the kept files' modification time (not their content).

```sh
# Change the files of a torrent that is already added (id from `taurus list`):
# the same list syntax as --files; at least one file, each index within the torrent
taurus torrent-select 4 0,2,5-7

# Give one torrent its own sharing rules; `default` goes back to the global setting
taurus job set 4 ratio 2.5            # a number, never, or default
taurus job set 4 seed_time 12h        # 30m, 2h, 1d, off, or default
taurus job set 4 ratio default
```

`torrent-select` says so when the torrent is queued again (run `taurus run` to download the new files) and fails, changing nothing, when the files do not fit on the disk. `job set` keeps the torrent's other rule as it was.

The torrent settings are changed like the others: `taurus config set torrent.ratio 2`.

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

`torrent.peers`, `torrent.port`, `torrent.upnp` and `torrent.dht` apply the next time Taurus starts. The others take effect at once, also for torrents that are running. The overall `speed` is shared evenly between web downloads and torrents while both run. A torrent's pieces are always downloaded in order.

`--name`, `--mirror` (repeatable) and `--checksum` apply to a single web download, so they need exactly one URL; `--files` needs exactly one torrent; `--dir`, `--connections` and `--referer` apply to every URL of the `add`.

### Video sites

`add` also takes links to video and audio sites (YouTube, Vimeo, Twitch and many more, read by [yt-dlp](https://github.com/yt-dlp/yt-dlp)), a playlist, or a live stream. The first time, Hyades downloads the tools it needs (see below), which takes a minute. `list` shows these as kind `media`; a playlist row shows `2 of 5 videos`, and a live stream shows `live 2m` while it records. A playlist is saved in a folder of its own inside `--dir`.

```sh
# The best video and sound, as one MP4
taurus add https://www.youtube.com/watch?v=VIDEO_ID

# Sound only: best audio, or a small MP3 with --quality small
taurus add https://www.youtube.com/watch?v=VIDEO_ID --audio
taurus add https://www.youtube.com/watch?v=VIDEO_ID --audio --quality small

# At most 720p: best, or any height the video has, such as 1080p, 720p or 480p (see
# taurus inspect); a quality the video lacks falls back to best, with a note.
# --quality audio is the same as --audio
taurus add https://vimeo.com/VIDEO_ID --quality 720p

# What a link offers, without adding it: the title, every quality, the subtitle
# languages and the captions the site generates (--json prints it as one object)
taurus inspect https://www.youtube.com/watch?v=VIDEO_ID
taurus --json inspect https://www.youtube.com/watch?v=VIDEO_ID --cookies-from firefox

# Convert the result: mp3, or smaller-mp4
taurus add https://www.youtube.com/watch?v=VIDEO_ID --convert smaller-mp4

# A playlist of more than 50 videos needs --yes
taurus add "https://www.youtube.com/playlist?list=PLAYLIST_ID" --yes

# A live stream records until it ends; stop it from another terminal and keep what was recorded
taurus add https://www.twitch.tv/CHANNEL
taurus stop 5
```

`taurus stop <ID>` stops a running video download or recording from another terminal, keeping what was downloaded (a live stream is made playable), and says how it ended. It asks the `taurus` process or the desktop app that runs the job, which answers within a moment; if none answers within 5 seconds it exits 1. Ctrl-C in the process that runs it also stops the job, but leaves it queued, not finished.

Video links also take these options:

```sh
# Sign in with a browser's cookies, for videos that need a login or a "not a bot" check
# (chrome, firefox, safari, edge, brave, chromium, opera or vivaldi, optionally :PROFILE)
taurus add https://www.youtube.com/watch?v=VIDEO_ID --cookies-from firefox
taurus add https://www.youtube.com/watch?v=VIDEO_ID --cookies-from chrome:"Profile 1"

# Retry a video that failed for want of a sign-in, with those cookies
taurus resume 5 --cookies-from firefox

# Subtitles: some languages (or all), with --auto-subs the captions the site generates,
# with --embed-subs also inside the video file (not for WebM or sound)
taurus add https://www.youtube.com/watch?v=VIDEO_ID --subs en,vi --auto-subs --embed-subs

# The file format of the video: mp4 (the default), mkv or webm
taurus add https://www.youtube.com/watch?v=VIDEO_ID --container mkv
```

`--auto-subs` and `--embed-subs` need `--subs`, and `--convert smaller-mp4` makes an MP4, so it cannot be used with `--container mkv` or `webm`. yt-dlp reads the browser's cookies itself; only the browser's name is stored with the download.

Convert a finished download, or any file that is not being downloaded, with ffmpeg:

```sh
# A finished download (a playlist makes one conversion per file), or a file by path
taurus convert 5 mp3
taurus convert ~/Music/talk.webm smaller-mp4
```

The result is a new file next to the original, `<name> (mp3).mp3` or `<name> (smaller).mp4` (a name that is taken gets ` (1)`, ` (2)`, ...); the original is never changed or overwritten. The conversion is a download of kind `convert` in the queue, and `convert` runs the queue until it is idle.

#### The tools

Video downloads use yt-dlp, ffmpeg and Deno. Hyades downloads them itself, checks each against its published SHA-256 and keeps them in `<data dir>/tools/<tool>/<version>/`; the file `<data dir>/tools/<tool>/current` names the version in use, and the previous version is kept for a rollback. They are not installed system-wide and the ones on your `PATH` are not used. yt-dlp is checked for a newer version once a day while videos download (sites change often), ffmpeg and Deno once a week.

```sh
taurus tools                    # version, state and path of each tool (--json: a JSON array; works while a download runs)
taurus tools install            # download what is missing, ahead of the first video
taurus tools update --force     # look for newer versions now
taurus tools rollback yt-dlp    # back to the previous version (also: ffmpeg, deno)
```

With `--json`, `install` and `update` print each tool event as a JSON line, then the final array; a tool's `state` is `"Missing"`, `"Ready"`, `{"Installing":{...}}` or `{"Failed":"<kind>"}`.

Licenses: yt-dlp is released under the Unlicense, the ffmpeg builds (from [yt-dlp/FFmpeg-Builds](https://github.com/yt-dlp/FFmpeg-Builds), and [ffmpeg.martin-riedl.de](https://ffmpeg.martin-riedl.de) on macOS) are GPL builds, and Deno is MIT. They are separate programs that Hyades starts; none is linked into it.

### Settings

`taurus config get` prints every setting as `key = value`; `taurus config get <KEY>` prints one. `taurus config set <KEY> <VALUE>` changes one:

| Key | Values | Default |
| --- | --- | --- |
| `max_active` | how many downloads run at once, at least 1 | `3` |
| `speed` | `fastest` (no limit), `balanced` (60 % of the fastest speed measured), `gentle` (25 %), or a fixed limit such as `500KB/s` or `5MB/s` (KB = 1000 bytes, MB = 1000000 bytes) | `fastest` |
| `when_done` | what to do once the queue is done (a download finishing empties it; pausing or removing the last one does not count): `nothing`, `notify` (prints `All downloads finished.`), `sleep` or `shutdown` (after a 60 second countdown; one-shot: once it runs, `when_done` goes back to `nothing`), `quit` | `nothing` |
| `auto_checksum` | `on` or `off`: verify a download against a `SHA256SUMS` file the host publishes beside it | `on` |
| `keep_awake` | `on` or `off`: keep the computer from sleeping while a download runs | `on` |
| `network.proxy` | how to reach the web: `system` (the OS settings; on Linux the `HTTP_PROXY`, `HTTPS_PROXY` and `NO_PROXY` variables), `none` (direct), or a proxy URL: `http://`, `https://`, `socks5://` or `socks5h://` (names resolved by the proxy) | `system` |

```sh
taurus config set max_active 2
taurus config set speed 5MB/s
taurus config set when_done shutdown
```

```sh
taurus config set network.proxy socks5h://127.0.0.1:9050
taurus config set network.proxy none
```

The proxy is used by downloads, yt-dlp and tool downloads from the next request on, and by torrents (a SOCKS proxy only) the next time Taurus starts; setting or leaving a SOCKS proxy prints `applies next time Taurus starts`. A proxy URL with a user name or password is refused (they would be stored in the settings), as is anything that is not one of the schemes above; `config set` then exits 1 and shows the reason.

Global options go before the command:

- `--json` prints every event as one JSON line on stdout; `list --json` prints one JSON array and `config get --json` one JSON object.
- `--data-dir <PATH>` sets where the job database lives. Without it, `TAURUS_DATA_DIR` is used, else the OS data directory.
- `--dry-run-power` prints the sleep or shutdown command of `when_done` (`would run: ...`) instead of running it.

## Desktop app (alpha)

Hyades also has a desktop app with a simple window: paste a link, see what it is, download it. It handles web downloads, torrents (magnet and `.torrent` links), and videos, audio and live streams from video sites, on Windows, macOS and Linux.

```sh
cargo run -p taurus-desktop
```

- **Data.** The app keeps its downloads list and settings in the same folder as the command-line client: `TAURUS_DATA_DIR` if set, else `~/.local/share/taurus` on Linux (`$XDG_DATA_HOME/taurus` when that is set), `~/Library/Application Support/dev.nikethai.taurus` on macOS and `%APPDATA%\nikethai\taurus\data` on Windows. Only one of them can use the folder at a time; opening the app while the command-line client runs shows a message and exits.
- **Tray.** Closing the window hides it to the tray and downloads go on; choose Quit in the tray menu to stop. On Linux the tray needs a StatusNotifier host (KDE Plasma has one; GNOME needs the AppIndicator extension). Without a tray, closing the window quits.
- **One window.** Launching the app again (for example `taurus-desktop <link>`, or with a `.torrent` file) shows the open window, from the tray or minimized, and hands it the links; other arguments are ignored.

Known alpha limitations: dragging links or files in from other apps does not work yet (the UI toolkit does not support it), and the colours and icons are provisional.
