# Hyades (codename Taurus)

Hyades (codename Taurus) is an open-source, cross-platform desktop download manager written in Rust, with a segmented HTTP engine, resumable downloads, and a native UI planned. Status: v2 rewrite in progress; see the [design spec](docs/superpowers/specs/2026-10-27-taurus-v2-design.md).

## Usage

The command-line client, `taurus`, drives the Hyades (codename Taurus) engine. It is a queue runner: `add`, `resume`, `run` and `start-now` queue their downloads, then run the whole queue in the foreground until nothing is queued, waiting or downloading (and until a `when_done` countdown fires or is cancelled). At most `max_active` downloads run at once; a download whose connection drops waits and retries on its own (after 1 minute, doubling up to 10 minutes). Ctrl-C stops the running downloads, keeping their progress, and leaves them queued with the rest; `taurus run` (or any other queue command) continues them. During a `when_done` countdown Ctrl-C cancels the countdown instead. Downloads interrupted by a crash, a kill or a reboot come back queued the same way and continue with the next queue run; `taurus list` shows them as Queued. Exit codes: 0 every download of the run completed, 1 a download failed or an error, 2 stopped by Ctrl-C or a download was paused.

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

# Show every download: id, state, percent, size, name
taurus list

# Forget download 1 and delete its partial or finished file
taurus remove 1 --delete-files
```

`--name`, `--mirror` (repeatable) and `--checksum` apply to a single download, so they need exactly one URL; `--dir`, `--connections` and `--referer` apply to every URL of the `add`.

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
