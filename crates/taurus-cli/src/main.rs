mod render;

use std::error::Error;
use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use taurus_core::{AddRequest, Engine, EngineConfig, Event, JobId, JobState, JobView};
use tokio::sync::broadcast::{self, error::RecvError, error::TryRecvError};

type Result<T> = std::result::Result<T, Box<dyn Error>>;

/// Segmented, resumable downloads.
#[derive(Parser)]
#[command(name = "taurus", version)]
struct Cli {
    /// Where the job database lives [default: $TAURUS_DATA_DIR, else the OS data dir]
    #[arg(long, value_name = "PATH")]
    data_dir: Option<PathBuf>,
    /// Print every event as one JSON line; `list` prints a JSON array
    #[arg(long)]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Download a URL in the foreground until done; Ctrl-C pauses
    Add {
        url: String,
        /// Destination folder [default: the Downloads folder, else the current dir]
        #[arg(long, value_name = "PATH")]
        dir: Option<PathBuf>,
        /// Maximum parallel connections [default: 16]
        #[arg(long, value_name = "N")]
        connections: Option<u32>,
        /// File name to save as, instead of the server's
        #[arg(long)]
        name: Option<String>,
    },
    /// Continue a paused or failed download in the foreground; Ctrl-C pauses
    Resume { id: i64 },
    /// Show every download: id, state, percent, size, name
    List,
    /// Forget a download, stopping it if it runs
    Remove {
        id: i64,
        /// Also delete its partial file, or the finished file
        #[arg(long)]
        delete_files: bool,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(e) => {
            let _ = e.print();
            // Clap would exit 2 on a usage error, which here means "paused".
            return if e.use_stderr() {
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            };
        }
    };
    match run(cli).await {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<ExitCode> {
    let data_dir = data_dir(cli.data_dir)?;
    let engine = match Engine::open(EngineConfig {
        data_dir: data_dir.clone(),
    })
    .await
    {
        Ok(engine) => engine,
        // Another process runs a download here: show what it has stored.
        Err(e) if e.is_instance_locked() && matches!(cli.command, Command::List) => {
            print_list(
                &Engine::read_snapshot(&EngineConfig { data_dir })?,
                cli.json,
            )?;
            return Ok(ExitCode::SUCCESS);
        }
        Err(e) => return Err(e.into()),
    };
    let result = execute(&engine, cli.command, cli.json).await;
    engine.shutdown().await?;
    result
}

async fn execute(engine: &Engine, command: Command, json: bool) -> Result<ExitCode> {
    let mut printer = Printer::new(json);
    match command {
        Command::Add {
            url,
            dir,
            connections,
            name,
        } => {
            let dest_dir = std::path::absolute(match dir {
                Some(dir) => dir,
                None => default_dest_dir()?,
            })?;
            let mut events = engine.subscribe();
            let id = engine
                .add(AddRequest {
                    url,
                    dest_dir,
                    file_name: name,
                    connections,
                })
                .await?;
            follow(engine, id, &mut events, &mut printer).await
        }
        Command::Resume { id } => {
            let id = JobId(id);
            let mut events = engine.subscribe();
            engine.resume(id).await?;
            follow(engine, id, &mut events, &mut printer).await
        }
        Command::List => {
            print_list(&engine.snapshot(), json)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Remove { id, delete_files } => {
            let id = JobId(id);
            let mut events = engine.subscribe();
            engine.remove(id, delete_files).await?;
            printer.drain(id, &mut events)?;
            if !json {
                println!("Removed {}", id.0);
            }
            Ok(ExitCode::SUCCESS)
        }
    }
}

fn print_list(views: &[JobView], json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string(views)?);
    } else {
        print!("{}", render::job_table(views));
    }
    Ok(())
}

/// Prints the job's events until it is Completed, Paused or Failed. Ctrl-C
/// pauses it.
async fn follow(
    engine: &Engine,
    id: JobId,
    events: &mut broadcast::Receiver<Event>,
    printer: &mut Printer,
) -> Result<ExitCode> {
    let wait = engine.wait(id);
    tokio::pin!(wait);
    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);
    let mut listening = true;
    // The engine never closes the channel while `engine` lives, so stop when
    // `wait` resolves instead of when `recv` fails.
    let state = loop {
        tokio::select! {
            state = &mut wait => break state,
            event = events.recv() => match event {
                Ok(event) => printer.event(id, &event)?,
                Err(RecvError::Lagged(_)) => {}
                Err(RecvError::Closed) => break (&mut wait).await,
            },
            signal = &mut ctrl_c, if listening => match signal {
                Ok(()) => {
                    engine.pause(id).await?;
                    break engine.wait(id).await;
                }
                // No Ctrl-C handler on this platform: keep downloading.
                Err(_) => listening = false,
            },
        }
    };
    // The final JobUpdated was sent before `wait` resolved.
    printer.drain(id, events)?;
    if let Some(view) = engine.snapshot().into_iter().find(|v| v.id == id) {
        printer.outcome(&view)?;
    }
    Ok(exit_code(state))
}

fn exit_code(state: JobState) -> ExitCode {
    match state {
        JobState::Completed => ExitCode::SUCCESS,
        JobState::Paused => ExitCode::from(2),
        JobState::Failed | JobState::Queued | JobState::Downloading => ExitCode::FAILURE,
    }
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

/// Writes one job's events to stdout: JSON lines with `--json`, otherwise a
/// progress line redrawn in place.
struct Printer {
    json: bool,
    /// Length of the progress line on screen, 0 if none.
    drawn: usize,
}

impl Printer {
    fn new(json: bool) -> Printer {
        Printer { json, drawn: 0 }
    }

    fn event(&mut self, id: JobId, event: &Event) -> Result<()> {
        let job = match event {
            Event::JobAdded(view) | Event::JobUpdated(view) => view.id,
            Event::Progress { id, .. } => *id,
            Event::JobRemoved(id) => *id,
        };
        if job != id {
            return Ok(());
        }
        let mut out = std::io::stdout().lock();
        if self.json {
            writeln!(out, "{}", serde_json::to_string(event)?)?;
        } else if let Event::Progress {
            downloaded,
            total,
            speed_bps,
            ..
        } = event
        {
            let line = render::progress_line(*downloaded, *total, *speed_bps);
            // Pad so a shorter line fully covers the previous one.
            write!(out, "\r{line:<width$}", width = self.drawn)?;
            self.drawn = line.len();
        }
        // Flushed per event: readers may be following a pipe live.
        out.flush()?;
        Ok(())
    }

    /// Prints the job's events that are already queued.
    fn drain(&mut self, id: JobId, events: &mut broadcast::Receiver<Event>) -> Result<()> {
        loop {
            match events.try_recv() {
                Ok(event) => self.event(id, &event)?,
                Err(TryRecvError::Lagged(_)) => {}
                Err(TryRecvError::Empty | TryRecvError::Closed) => return Ok(()),
            }
        }
    }

    /// Ends the progress line and prints how the download ended (human mode
    /// only; in JSON mode the final JobUpdated says it).
    fn outcome(&mut self, view: &JobView) -> Result<()> {
        if self.json {
            return Ok(());
        }
        let mut out = std::io::stdout().lock();
        if self.drawn > 0 {
            writeln!(out)?;
            self.drawn = 0;
        }
        writeln!(out, "{}", render::outcome_line(view))?;
        Ok(())
    }
}
