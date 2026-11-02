//! Commands that change one download: `login`, `convert`, `torrent-select`
//! and `job set`.

use std::path::Path;
use std::process::ExitCode;

use clap::{Args, Subcommand, ValueEnum};
use taurus_core::media::ConvertPreset;
use taurus_core::{ConvertSource, Engine, JobId, JobState, JobView};

use crate::file_list::FileList;
use crate::runner::{Enqueued, Options, Runner};
use crate::video::Convert;
use crate::{Result, config, password};

#[derive(Args, Clone, Debug)]
pub struct LoginArgs {
    pub id: i64,
    /// The user name; the password is asked for on the terminal
    #[arg(long, value_name = "NAME")]
    pub user: String,
}

#[derive(Args, Clone, Debug)]
pub struct ConvertArgs {
    /// A finished download's id, or the path of a file
    #[arg(value_name = "ID|PATH")]
    pub source: String,
    #[arg(value_enum)]
    pub to: Convert,
}

#[derive(Args, Clone, Debug)]
pub struct SelectArgs {
    pub id: i64,
    /// The files to download, by index: 0,2,5-7 (see torrent-files)
    #[arg(value_name = "LIST")]
    pub files: FileList,
}

#[derive(Subcommand, Clone, Debug)]
pub enum JobAction {
    /// Change a torrent's own sharing rule: ratio <N>|never|default, or
    /// seed_time 30m|2h|1d|off|default (default follows the global setting)
    Set {
        id: i64,
        #[arg(value_enum)]
        key: JobKey,
        value: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum JobKey {
    Ratio,
    #[value(name = "seed_time", alias = "seed-time")]
    SeedTime,
}

fn find(engine: &Engine, id: JobId) -> Result<JobView> {
    engine
        .snapshot()
        .into_iter()
        .find(|v| v.id == id)
        .ok_or_else(|| format!("download {} not found", id.0).into())
}

/// Gives the job its credentials (it resumes when it was stopped), then
/// runs the queue until it is idle.
pub async fn login(engine: &Engine, args: LoginArgs, options: Options) -> Result<ExitCode> {
    let id = JobId(args.id);
    find(engine, id)?;
    let creds = password::credentials(&args.user)?;
    let runner = Runner::new(engine, options);
    let enqueue = async {
        let mut enqueued = Enqueued::default();
        match sign_in(engine, id, creds).await {
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

async fn sign_in(
    engine: &Engine,
    id: JobId,
    creds: taurus_core::Credentials,
) -> std::result::Result<(), taurus_core::CoreError> {
    // A job that failed to sign in resumes by itself.
    engine.set_credentials(id, creds).await?;
    let stopped = engine
        .snapshot()
        .into_iter()
        .any(|v| v.id == id && matches!(v.state, JobState::Paused | JobState::Failed));
    if stopped {
        engine.resume(id).await?;
    }
    Ok(())
}

/// Queues the conversion of a finished download or of a file, then runs the
/// queue until it is idle.
pub async fn convert(engine: &Engine, args: ConvertArgs, options: Options) -> Result<ExitCode> {
    let source = match args.source.parse::<i64>() {
        // A file named like a number is still a file.
        Ok(id) if !Path::new(&args.source).exists() => ConvertSource::Job(JobId(id)),
        _ => ConvertSource::Path(std::path::absolute(&args.source)?),
    };
    let preset = match args.to {
        Convert::Mp3 => ConvertPreset::Mp3,
        Convert::SmallerMp4 => ConvertPreset::SmallerMp4,
    };
    let runner = Runner::new(engine, options);
    let enqueue = async {
        match engine.convert_many(source, preset).await {
            Ok(ids) => Enqueued { ids, failed: false },
            Err(e) => {
                eprintln!("error: {e}");
                Enqueued {
                    ids: Vec::new(),
                    failed: true,
                }
            }
        }
    };
    runner.run(enqueue).await
}

/// Changes the files of a torrent that are downloaded.
pub async fn torrent_select(engine: &Engine, args: SelectArgs, json: bool) -> Result<ExitCode> {
    let id = JobId(args.id);
    let FileList(files) = args.files;
    engine.set_torrent_files(id, files.clone()).await?;
    let state = find(engine, id)?.state;
    if json {
        println!(
            "{}",
            serde_json::json!({ "id": id.0, "files": files, "state": state })
        );
        return Ok(ExitCode::SUCCESS);
    }
    let count = files.len();
    println!(
        "Download {}: {count} file{} selected.",
        id.0,
        if count == 1 { "" } else { "s" }
    );
    if state == JobState::Queued {
        println!("Run taurus run to download them.");
    }
    Ok(ExitCode::SUCCESS)
}

/// The override for `value`: `default` clears it (follows the global
/// setting), anything else is read by `parse`.
fn parse_override<T>(
    value: &str,
    parse: impl Fn(&str) -> std::result::Result<Option<T>, String>,
) -> std::result::Result<Option<Option<T>>, String> {
    if value.trim().eq_ignore_ascii_case("default") {
        return Ok(None);
    }
    parse(value).map(Some)
}

/// Changes one sharing rule of a torrent, keeping the other.
pub async fn job_set(engine: &Engine, action: JobAction, json: bool) -> Result<ExitCode> {
    let JobAction::Set { id, key, value } = action;
    let id = JobId(id);
    let view = find(engine, id)?;
    let Some(torrent) = view.torrent else {
        return Err(format!("download {} is not a torrent", id.0).into());
    };
    let mut overrides = torrent.overrides;
    let (name, shown) = match key {
        JobKey::Ratio => {
            overrides.stop_ratio = parse_override(&value, config::parse_ratio)?;
            let shown = match overrides.stop_ratio {
                None => "default".to_string(),
                Some(None) => "never".to_string(),
                Some(Some(ratio)) => format!("{ratio:?}"),
            };
            ("ratio", shown)
        }
        JobKey::SeedTime => {
            overrides.stop_after_secs = parse_override(&value, config::parse_duration)?;
            let shown = match overrides.stop_after_secs {
                None => "default".to_string(),
                Some(None) => "off".to_string(),
                Some(Some(secs)) => config::format_duration(secs),
            };
            ("seed_time", shown)
        }
    };
    engine.set_torrent_overrides(id, overrides.clone()).await?;
    if json {
        println!(
            "{}",
            serde_json::json!({ "id": id.0, "overrides": overrides })
        );
    } else {
        println!("Download {}: {name} = {shown}", id.0);
    }
    Ok(ExitCode::SUCCESS)
}
