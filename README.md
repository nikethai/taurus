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

# Show every download: id, state, kind (http or torrent), percent, size, name
taurus list

# Forget download 1 and delete its partial or finished file
taurus remove 1 --delete-files
```

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

### Settings

`taurus config get` prints every setting as `key = value`; `taurus config get <KEY>` prints one. `taurus config set <KEY> <VALUE>` changes one:

| Key | Values | Default |
| --- | --- | --- |
| `max_active` | how many downloads run at once, at least 1 | `3` |
| `speed` | `fastest` (no limit), `balanced` (60 % of the fastest speed measured), `gentle` (25 %), or a fixed limit such as `500KB/s` or `5MB/s` (KB = 1000 bytes, MB = 1000000 bytes) | `fastest` |
| `when_done` | what to do once the queue is done (a download finishing empties it; pausing or removing the last one does not count): `nothing`, `notify` (prints `All downloads finished.`), `sleep` or `shutdown` (after a 60 second countdown; one-shot: once it runs, `when_done` goes back to `nothing`), `quit` | `nothing` |
| `auto_checksum` | `on` or `off`: verify a download against a `SHA256SUMS` file the host publishes beside it | `on` |
| `keep_awake` | `on` or `off`: keep the computer from sleeping while a download runs | `on` |

```sh
taurus config set max_active 2
taurus config set speed 5MB/s
taurus config set when_done shutdown
```

Global options go before the command:

- `--json` prints every event as one JSON line on stdout; `list --json` prints one JSON array and `config get --json` one JSON object.
- `--data-dir <PATH>` sets where the job database lives. Without it, `TAURUS_DATA_DIR` is used, else the OS data directory.
- `--dry-run-power` prints the sleep or shutdown command of `when_done` (`would run: ...`) instead of running it.

## Desktop app (alpha)

Hyades also has a desktop app with a simple window: paste a link, see what it is, download it. It handles web downloads and torrents (magnet and `.torrent` links) on Windows, macOS and Linux.

```sh
cargo run -p taurus-desktop
```

- **Data.** The app keeps its downloads list and settings in the same folder as the command-line client: `TAURUS_DATA_DIR` if set, else `~/.local/share/taurus` on Linux (`$XDG_DATA_HOME/taurus` when that is set), `~/Library/Application Support/dev.nikethai.taurus` on macOS and `%APPDATA%\nikethai\taurus\data` on Windows. Only one of them can use the folder at a time; opening the app while the command-line client runs shows a message and exits.
- **Tray.** Closing the window hides it to the tray and downloads go on; choose Quit in the tray menu to stop. On Linux the tray needs a StatusNotifier host (KDE Plasma has one; GNOME needs the AppIndicator extension). Without a tray, closing the window quits.
- **One window.** Launching the app again (for example `taurus-desktop <link>`, or with a `.torrent` file) shows the open window, from the tray or minimized, and hands it the links; other arguments are ignored.

Known alpha limitations: dragging links or files in from other apps does not work yet (the UI toolkit does not support it), and the colours and icons are provisional.
