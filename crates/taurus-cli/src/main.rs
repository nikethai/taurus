mod config;
mod file_list;
mod render;
mod runner;
mod stop;
mod tools;
mod video;

use clap::{CommandFactory, Parser, Subcommand};
use std::error::Error;
use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use taurus_core::tools::ToolSources;
use taurus_core::torrent::hub::TorrentTestNet;
use taurus_core::torrent::source::{Source, classify};
use taurus_core::{AddRequest, Engine, EngineConfig, JobId, JobView, PowerMode, Timing};
use tokio::sync::broadcast::error::TryRecvError;

use crate::file_list::FileList;
use crate::runner::{Enqueued, Options, Runner};
use crate::tools::ToolsAction;
use crate::video::VideoFlags;

type Result<T> = std::result::Result<T, Box<dyn Error>>;

/// Segmented, resumable downloads.
#[derive(Parser)]
#[command(name = "taurus", version)]
struct Cli {
    /// Where the job database lives [default: $TAURUS_DATA_DIR, else the OS data dir]
    #[arg(long, value_name = "PATH")]
    data_dir: Option<PathBuf>,
    /// Print every event as one JSON line; `list` and `config get` print JSON
    #[arg(long)]
    json: bool,
    /// Print the sleep or shutdown command of `when_done` instead of running it
    #[arg(long)]
    dry_run_power: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Queue downloads, then run the queue in the foreground until it is idle; Ctrl-C stops (taurus run continues)
    Add {
        /// Web links, magnet links, .torrent files or .torrent links
        #[arg(
            value_name = "URL|MAGNET|FILE.torrent",
            required_unless_present = "file"
        )]
        urls: Vec<String>,
        /// Also add the URLs in this file, one per line (blank lines and `#` lines are skipped)
        #[arg(long, value_name = "PATH")]
        file: Option<PathBuf>,
        /// Destination folder [default: the Downloads folder, else the current dir]
        #[arg(long, value_name = "PATH")]
        dir: Option<PathBuf>,
        /// Maximum parallel connections per download [default: 16]
        #[arg(long, value_name = "N")]
        connections: Option<u32>,
        /// File name to save as, instead of the server's (one URL only)
        #[arg(long)]
        name: Option<String>,
        /// Another URL serving the same file; repeatable (one URL only)
        #[arg(long = "mirror", value_name = "URL")]
        mirrors: Vec<String>,
        /// Expected SHA-256 of the file, checked once downloaded (one URL only)
        #[arg(long, value_name = "SUM")]
        checksum: Option<String>,
        /// Sent as the Referer of every request
        #[arg(long, value_name = "URL")]
        referer: Option<String>,
        /// The files of a torrent to download, by index: 0,2,5-7 (one torrent only; see torrent-files) [default: all]
        #[arg(long, value_name = "LIST")]
        files: Option<FileList>,
        #[command(flatten)]
        video: VideoFlags,
    },
    /// Continue paused or failed downloads, then run the queue until it is idle; Ctrl-C stops (taurus run continues)
    Resume {
        #[arg(value_name = "ID", required = true)]
        ids: Vec<i64>,
    },
    /// Run the queued and waiting downloads until the queue is idle; Ctrl-C stops (taurus run continues)
    Run,
    /// Start a queued or waiting download at once, even past max_active, then run the queue
    StartNow { id: i64 },
    /// Show every download: id, state, kind (http, torrent or media), percent, size, name
    List,
    /// Forget a download, stopping it if it runs
    Remove {
        id: i64,
        /// Also delete its partial file, or the finished file
        #[arg(long)]
        delete_files: bool,
    },
    /// List the files of a torrent (magnet link, .torrent file or link) with their indices, without adding it
    TorrentFiles {
        #[arg(value_name = "MAGNET|FILE.torrent|URL")]
        input: String,
    },
    /// Stop a live recording and keep what was recorded (it must be running in another taurus process)
    Stop { id: i64 },
    /// Show or install the tools that video downloads use (yt-dlp, ffmpeg, Deno)
    Tools {
        #[command(subcommand)]
        action: Option<ToolsAction>,
    },
    /// Show or change the settings
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
}

#[derive(Subcommand)]
enum ConfigAction {
    /// Print every setting, or one, as `key = value`
    Get { key: Option<String> },
    /// Change a setting: max_active <N> | speed fastest|balanced|gentle|<N>KB/s|<N>MB/s |
    /// when_done nothing|notify|sleep|shutdown|quit (sleep and shutdown run once, then reset
    /// to nothing) | auto_checksum on|off | keep_awake on|off |
    /// torrent.ratio <N>|never | torrent.seed_time 30m|2h|1d|off |
    /// torrent.upload_limit off|<N>KB/s|<N>MB/s | torrent.download_limit (same) |
    /// torrent.max_active <N> | torrent.peers <N>|default | torrent.port 1024-65535|random |
    /// torrent.upnp on|off | torrent.dht on|off
    Set { key: String, value: String },
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = match parse() {
        Ok(cli) => cli,
        Err(code) => return code,
    };
    match run(cli).await {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Parses the arguments and folds `add --file` into the URLs.
fn parse() -> std::result::Result<Cli, ExitCode> {
    let mut cli = Cli::try_parse().map_err(|e| usage_error(&e))?;
    if let Command::Add {
        urls,
        file,
        name,
        mirrors,
        checksum,
        files,
        video,
        ..
    } = &mut cli.command
    {
        if let Some(file) = file.take() {
            match read_url_file(&file) {
                Ok(more) => urls.extend(more),
                Err(e) => {
                    eprintln!("error: cannot read {}: {e}", file.display());
                    return Err(ExitCode::FAILURE);
                }
            }
        }
        if urls.is_empty() {
            let e = Cli::command().error(
                clap::error::ErrorKind::MissingRequiredArgument,
                "no URL to add",
            );
            return Err(usage_error(&e));
        }
        if urls.len() > 1 && (name.is_some() || !mirrors.is_empty() || checksum.is_some()) {
            let e = Cli::command().error(
                clap::error::ErrorKind::ArgumentConflict,
                "--name, --mirror and --checksum can only be used with exactly one URL",
            );
            return Err(usage_error(&e));
        }
        if files.is_some() && !(urls.len() == 1 && is_torrent(&urls[0])) {
            let e = Cli::command().error(
                clap::error::ErrorKind::ArgumentConflict,
                "--files can only be used with exactly one torrent (magnet link, .torrent file or link)",
            );
            return Err(usage_error(&e));
        }
        if let Some(message) = video.conflict() {
            let e = Cli::command().error(clap::error::ErrorKind::ArgumentConflict, message);
            return Err(usage_error(&e));
        }
    }
    Ok(cli)
}

fn is_torrent(input: &str) -> bool {
    matches!(
        classify(input),
        Ok(Source::Magnet { .. } | Source::TorrentUrl(_) | Source::TorrentFile(_))
    )
}

/// Prints a clap error; usage errors exit 1, since 2 means "stopped".
fn usage_error(e: &clap::Error) -> ExitCode {
    let _ = e.print();
    if e.use_stderr() {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

/// One URL per line, skipping blank lines and lines starting with `#`.
fn read_url_file(path: &Path) -> std::io::Result<Vec<String>> {
    Ok(std::fs::read_to_string(path)?
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(String::from)
        .collect())
}

async fn run(cli: Cli) -> Result<ExitCode> {
    let data_dir = data_dir(cli.data_dir)?;
    // Only the queue runners start jobs; the other commands look or edit.
    let autostart = matches!(
        cli.command,
        Command::Add { .. } | Command::Resume { .. } | Command::Run | Command::StartNow { .. }
    );
    let config = engine_config(data_dir.clone(), cli.dry_run_power, autostart);
    let options = Options {
        json: cli.json,
        dry_run: cli.dry_run_power,
        timing: config.timing.clone(),
        data_dir: data_dir.clone(),
    };
    let engine = match Engine::open(config).await {
        Ok(engine) => engine,
        // Another process runs downloads here: show what it has stored.
        Err(e) if e.is_instance_locked() && matches!(cli.command, Command::List) => {
            print_list(
                &Engine::read_snapshot(&EngineConfig::new(data_dir))?,
                cli.json,
            )?;
            return Ok(ExitCode::SUCCESS);
        }
        // The recording to stop runs in that other process: ask it.
        Err(e) if e.is_instance_locked() => {
            if let Command::Stop { id } = cli.command {
                return stop::request(data_dir, JobId(id), cli.json).await;
            }
            return Err(e.into());
        }
        Err(e) => return Err(e.into()),
    };
    let result = execute(&engine, cli.command, options).await;
    engine.shutdown().await?;
    result
}

fn engine_config(data_dir: PathBuf, dry_run_power: bool, autostart: bool) -> EngineConfig {
    let mut config = EngineConfig::new(data_dir);
    config.autostart = autostart;
    if dry_run_power {
        config.power = PowerMode::DryRun;
    }
    // Test-only switch: short retry and when-done delays, so the CLI's
    // integration tests do not wait in real time. Not for users.
    if std::env::var_os("TAURUS_FAST_TIMING").is_some_and(|v| v == "1") {
        config.timing = Timing::fast_for_tests();
    }
    // Test-only switch: `host:port,...` are the peers every torrent gets, and
    // the torrent session leaves DHT, trackers, LSD and UPnP off. Not for
    // users.
    if let Some(peers) = std::env::var_os("TAURUS_TORRENT_TEST_PEERS") {
        config.torrent_test = Some(TorrentTestNet {
            initial_peers: peers
                .to_string_lossy()
                .split(',')
                .filter_map(|peer| peer.trim().parse::<SocketAddr>().ok())
                .collect(),
            restore_in_place: false,
        });
    }
    // Test-only switch: the tool manager downloads yt-dlp, ffmpeg and Deno
    // from the fake host at this base URL. Not for users.
    if let Some(base) = std::env::var_os("TAURUS_TOOL_SOURCES") {
        match url::Url::parse(&base.to_string_lossy()) {
            Ok(base) => config.tool_sources = ToolSources::under(&base),
            Err(e) => eprintln!("warning: ignoring TAURUS_TOOL_SOURCES: {e}"),
        }
    }
    config
}

async fn execute(engine: &Engine, command: Command, options: Options) -> Result<ExitCode> {
    let json = options.json;
    match command {
        Command::Add {
            urls,
            file: _,
            dir,
            connections,
            name,
            mirrors,
            checksum,
            referer,
            files,
            video,
        } => {
            let only_files = files.map(|FileList(indices)| indices);
            let dest_dir = std::path::absolute(match dir {
                Some(dir) => dir,
                None => default_dest_dir()?,
            })?;
            let runner = Runner::new(engine, options);
            let enqueue = async {
                // A job may finish while a later URL still resolves: no
                // when-done action before the whole batch is queued.
                let hold = engine.hold_when_done();
                let mut enqueued = Enqueued::default();
                for url in urls {
                    let req = AddRequest {
                        url: url.clone(),
                        dest_dir: dest_dir.clone(),
                        file_name: name.clone(),
                        connections,
                        mirrors: mirrors.clone(),
                        checksum: checksum.clone(),
                        referer: referer.clone(),
                        only_files: only_files.clone(),
                        ..AddRequest::default()
                    };
                    let torrent = is_torrent(&url);
                    match Box::pin(video::add(engine, req, &video, torrent)).await {
                        Ok(id) => enqueued.ids.push(id),
                        Err(e) => {
                            eprintln!("error: {url}: {}", video::error_text(&e));
                            enqueued.failed = true;
                        }
                    }
                }
                drop(hold);
                enqueued
            };
            runner.run(enqueue).await
        }
        Command::Resume { ids } => {
            let runner = Runner::new(engine, options);
            let enqueue = async {
                let hold = engine.hold_when_done();
                let mut enqueued = Enqueued::default();
                for id in ids.into_iter().map(JobId) {
                    match engine.resume(id).await {
                        Ok(()) => enqueued.ids.push(id),
                        Err(e) => {
                            eprintln!("error: download {}: {e}", id.0);
                            enqueued.failed = true;
                        }
                    }
                }
                drop(hold);
                enqueued
            };
            runner.run(enqueue).await
        }
        Command::Run => {
            let runner = Runner::new(engine, options);
            if !runner.has_work() {
                if !json {
                    println!("Nothing to do.");
                }
                return Ok(ExitCode::SUCCESS);
            }
            runner.run(async { Enqueued::default() }).await
        }
        Command::StartNow { id } => {
            let id = JobId(id);
            let runner = Runner::new(engine, options);
            let enqueue = async {
                let mut enqueued = Enqueued::default();
                match engine.start_now(id).await {
                    Ok(()) => enqueued.ids.push(id),
                    Err(e) => {
                        eprintln!("error: download {}: {e}", id.0);
                        enqueued.failed = true;
                    }
                }
                enqueued
            };
            runner.run(enqueue).await
        }
        Command::List => {
            print_list(&engine.snapshot(), json)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Remove { id, delete_files } => {
            let id = JobId(id);
            let mut events = engine.subscribe();
            engine.remove(id, delete_files).await?;
            if json {
                let mut out = std::io::stdout().lock();
                loop {
                    match events.try_recv() {
                        Ok(event) => writeln!(out, "{}", serde_json::to_string(&event)?)?,
                        Err(TryRecvError::Lagged(_)) => {}
                        Err(TryRecvError::Empty | TryRecvError::Closed) => break,
                    }
                }
            } else {
                println!("Removed {}", id.0);
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::TorrentFiles { input } => {
            let info = engine.torrent_files(&input).await?;
            if json {
                println!("{}", serde_json::to_string(&info)?);
            } else {
                print!("{}", render::torrent_files(&info));
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Stop { id } => stop::local(engine, JobId(id)),
        Command::Tools { action } => tools::run(engine, action, json).await,
        Command::Config { action } => configure(engine, action, json).await,
    }
}

async fn configure(engine: &Engine, action: ConfigAction, json: bool) -> Result<ExitCode> {
    let mut settings = engine.settings();
    match action {
        ConfigAction::Get { key: None } => {
            if json {
                println!("{}", serde_json::to_string(&settings)?);
            } else {
                print!("{}", config::lines(&settings));
            }
        }
        ConfigAction::Get { key: Some(key) } => {
            let value = config::get(&settings, &key)?;
            if json {
                let all = serde_json::to_value(&settings)?;
                let field = match config::json_field(&key) {
                    Some((section, name)) => &all[section][name],
                    None => &all[key.as_str()],
                };
                println!("{}", serde_json::json!({ key.as_str(): field }));
            } else {
                println!("{key} = {value}");
            }
        }
        ConfigAction::Set { key, value } => {
            config::set(&mut settings, &key, &value)?;
            engine.set_settings(settings.clone()).await?;
            if json {
                println!("{}", serde_json::to_string(&settings)?);
            } else {
                println!("{key} = {}", config::get(&settings, &key)?);
                if config::RESTART_KEYS.contains(&key.as_str()) {
                    println!("{}", config::RESTART_NOTE);
                }
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn print_list(views: &[JobView], json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string(views)?);
    } else {
        print!("{}", render::job_table(views));
    }
    Ok(())
}

/// `--data-dir`, else `TAURUS_DATA_DIR`, else the OS data dir.
fn data_dir(flag: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(dir) = flag {
        return Ok(dir);
    }
    if let Some(dir) = std::env::var_os("TAURUS_DATA_DIR").filter(|d| !d.is_empty()) {
        return Ok(dir.into());
    }
    directories::ProjectDirs::from("dev", "nikethai", "taurus")
        .map(|dirs| dirs.data_dir().to_path_buf())
        .ok_or_else(|| "no data directory found; pass --data-dir or set TAURUS_DATA_DIR".into())
}

fn default_dest_dir() -> Result<PathBuf> {
    match directories::UserDirs::new().and_then(|u| u.download_dir().map(PathBuf::from)) {
        Some(dir) => Ok(dir),
        None => Ok(std::env::current_dir()?),
    }
}
