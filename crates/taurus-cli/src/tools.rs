//! `hyades-cli tools`: the yt-dlp, ffmpeg and Deno that video downloads use.

use std::future::Future;
use std::process::ExitCode;

use clap::{Subcommand, ValueEnum};
use taurus_core::tools::{ToolKind, ToolState, ToolStatus};
use taurus_core::{Engine, Event};

use crate::Result;

#[derive(Subcommand)]
pub enum ToolsAction {
    /// Show each tool's version, path and state (the default)
    Status,
    /// Download every tool that is missing
    Install,
    /// Look for newer versions (at most daily for yt-dlp, weekly for the others) and install them
    Update {
        /// Check now, whatever the last check was
        #[arg(long)]
        force: bool,
    },
    /// Go back to the version that was installed before the current one
    Rollback {
        #[arg(value_enum)]
        tool: Tool,
    },
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum Tool {
    #[value(name = "yt-dlp")]
    YtDlp,
    Ffmpeg,
    Deno,
}

impl Tool {
    fn kind(self) -> ToolKind {
        match self {
            Tool::YtDlp => ToolKind::YtDlp,
            Tool::Ffmpeg => ToolKind::Ffmpeg,
            Tool::Deno => ToolKind::Deno,
        }
    }
}

pub async fn run(engine: &Engine, action: Option<ToolsAction>, json: bool) -> Result<ExitCode> {
    match action.unwrap_or(ToolsAction::Status) {
        ToolsAction::Status => {}
        ToolsAction::Install => follow(engine, engine.install_tools(), json).await?,
        ToolsAction::Update { force } => follow(engine, engine.update_tools(force), json).await?,
        ToolsAction::Rollback { tool } => {
            let kind = tool.kind();
            engine.rollback_tool(kind).await?;
            if !json && let Some(status) = engine.tools_status().iter().find(|s| s.kind == kind) {
                println!(
                    "Rolled back {} to {}",
                    kind.name(),
                    status.version.as_deref().unwrap_or("?")
                );
                return Ok(ExitCode::SUCCESS);
            }
        }
    }
    print_status(&engine.tools_status(), json)?;
    Ok(ExitCode::SUCCESS)
}

/// The tools as a table, or as one JSON array.
pub fn print_status(statuses: &[ToolStatus], json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string(statuses)?);
    } else {
        print!("{}", table(statuses));
    }
    Ok(())
}

/// Runs `work` while printing the tools' progress: every event as a JSON
/// line with `--json`, otherwise one line when a tool starts and one when it
/// ends.
async fn follow(
    engine: &Engine,
    work: impl Future<Output = std::result::Result<(), taurus_core::CoreError>>,
    json: bool,
) -> Result<()> {
    let mut events = engine.subscribe();
    tokio::pin!(work);
    let mut announced = Vec::new();
    let result = loop {
        tokio::select! {
            result = &mut work => break result,
            event = events.recv() => {
                if let Ok(event) = event {
                    report(&event, json, &mut announced)?;
                }
            }
        }
    };
    while let Ok(event) = events.try_recv() {
        report(&event, json, &mut announced)?;
    }
    Ok(result?)
}

fn report(event: &Event, json: bool, announced: &mut Vec<ToolKind>) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string(event)?);
    } else if let Event::ToolStatus(status) = event {
        let name = status.kind.name();
        match &status.state {
            ToolState::Installing { .. } if !announced.contains(&status.kind) => {
                announced.push(status.kind);
                println!("Downloading {name}...");
            }
            ToolState::Ready if announced.contains(&status.kind) => {
                println!(
                    "Installed {name} {}",
                    status.version.as_deref().unwrap_or("?")
                );
            }
            ToolState::Failed(kind) => println!("{name}: failed ({kind:?})"),
            _ => {}
        }
    }
    Ok(())
}

/// One line per tool: name, version, state, path.
fn table(statuses: &[ToolStatus]) -> String {
    let mut out = format!("{:<8}  {:<14}  {:<12}  PATH\n", "TOOL", "VERSION", "STATE");
    for s in statuses {
        let state = match &s.state {
            ToolState::Missing => "Missing".to_string(),
            ToolState::Installing { .. } => "Installing".to_string(),
            ToolState::Ready => "Ready".to_string(),
            ToolState::Failed(kind) => format!("Failed ({kind:?})"),
        };
        out.push_str(&format!(
            "{:<8}  {:<14}  {:<12}  {}\n",
            s.kind.name(),
            s.version.as_deref().unwrap_or("-"),
            state,
            s.path
                .as_ref()
                .map_or("-".to_string(), |p| p.display().to_string()),
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_lists_every_tool() {
        let statuses = vec![
            ToolStatus {
                kind: ToolKind::YtDlp,
                version: Some("2026.01.01".into()),
                path: Some("/d/tools/yt-dlp/2026.01.01/yt-dlp".into()),
                state: ToolState::Ready,
            },
            ToolStatus {
                kind: ToolKind::Deno,
                version: None,
                path: None,
                state: ToolState::Missing,
            },
        ];
        let text = table(&statuses);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[1].starts_with("yt-dlp    2026.01.01 "), "{text}");
        assert!(
            lines[1].ends_with("/d/tools/yt-dlp/2026.01.01/yt-dlp"),
            "{text}"
        );
        assert!(
            lines[2].contains("deno") && lines[2].contains("Missing"),
            "{text}"
        );
    }
}
