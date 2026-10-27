# Hyades (codename Taurus)

Hyades (codename Taurus) is an open-source, cross-platform desktop download manager written in Rust, with a segmented HTTP engine, resumable downloads, and a native UI planned. Status: v2 rewrite in progress; see the [design spec](docs/superpowers/specs/2026-10-27-taurus-v2-design.md).

## Usage

The command-line client, `taurus`, drives the Hyades (codename Taurus) engine. `add` and `resume` run in the foreground until the download ends; Ctrl-C pauses it. Exit codes: 0 completed, 2 paused, 1 failed or error.

```sh
# Download into a folder (default: your Downloads folder), with up to 8 connections
taurus add https://example.com/file.iso --dir ~/Downloads --connections 8

# Continue download 1 after a pause, crash or reboot
taurus resume 1

# Show every download: id, state, percent, size, name
taurus list

# Forget download 1 and delete its partial or finished file
taurus remove 1 --delete-files
```

Global options go before the command:

- `--json` prints every event as one JSON line on stdout; `list --json` prints one JSON array.
- `--data-dir <PATH>` sets where the job database lives. Without it, `TAURUS_DATA_DIR` is used, else the OS data directory.
