mod config;
mod render;
mod runner;

use std::error::Error;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{CommandFactory, Parser, Subcommand};
use taurus_core::{AddRequest, Engine, EngineConfig, JobId, JobView, PowerMode, Timing};
use tokio::sync::broadcast::error::TryRecvError;

use crate::runner::{Enqueued, Options, Runner};

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
        #[arg(value_name = "URL", required_unless_present = "file")]
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
    /// Show every download: id, state, percent, size, name
    List,
    /// Forget a download, stopping it if it runs
    Remove {
        id: i64,
        /// Also delete its partial file, or the finished file
        #[arg(long)]
        delete_files: bool,
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
    /// to nothing) | auto_checksum on|off | keep_awake on|off
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
    }
    Ok(cli)
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
        } => {
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
                    };
                    match engine.add(req).await {
                        Ok(id) => enqueued.ids.push(id),
                        Err(e) => {
                            eprintln!("error: {url}: {e}");
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
                println!("{}", serde_json::json!({ key.as_str(): all[key.as_str()] }));
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
