mod brightness;
mod config;
mod health;
mod hermes;
mod mask;
mod mcp;
mod metrics;
mod reconcile;
mod store;
mod tools;
mod ui;

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use store::Store;
use tools::ToolError;

/// Full-screen dashboard for Hermes Agent.
///
/// Without a subcommand, runs the dashboard GUI.
#[derive(Debug, Parser)]
#[command(name = "hermes-beaver", version, about)]
struct Cli {
    /// Config file (default: ~/.config/hermes-beaver/config.toml)
    #[arg(long, global = true, value_name = "PATH")]
    config: Option<PathBuf>,

    /// Event store (default: ~/.local/state/hermes-beaver/events.db)
    #[arg(long, global = true, value_name = "PATH")]
    store: Option<PathBuf>,

    /// Development aid: save a PPM snapshot of the window after startup, then exit.
    #[arg(long, global = true, value_name = "PATH", hide = true)]
    screenshot: Option<PathBuf>,

    /// Development aid: run in a 1920x1080 window instead of full-screen.
    #[arg(long, global = true, hide = true)]
    windowed: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the MCP server on stdio (spawned by Hermes).
    Mcp,
    /// Run one tool and print its JSON result, for scripts.
    ///
    /// Exit status: 0 on success, 2 for invalid arguments or unknown tools,
    /// 1 for internal errors.
    Call {
        /// Tool name, e.g. task_start
        tool: String,
        /// Tool arguments as a JSON object
        json: String,
    },
    /// Run the GUI against synthetic data, without touching Hermes.
    Demo {
        /// Show one scenario instead of cycling: busy, idle, overflow, broken, empty
        #[arg(long)]
        scenario: Option<String>,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let config = config::Config::load(cli.config.as_deref())?;
    let store_path = cli.store.clone().unwrap_or_else(Store::default_path);

    match cli.command {
        None => ui::run(
            &config,
            ui::Mode::Live,
            &store_path,
            cli.screenshot.as_deref(),
            cli.windowed,
        ),
        Some(Command::Demo { scenario }) => ui::run(
            &config,
            ui::Mode::Demo(scenario),
            &store_path,
            cli.screenshot.as_deref(),
            cli.windowed,
        ),
        Some(Command::Mcp) => {
            let mut store = Store::open(&store_path)?;
            mcp::serve(&mut store)
        }
        Some(Command::Call { tool, json }) => {
            std::process::exit(run_call(&store_path, &tool, &json))
        }
    }
}

fn run_call(store_path: &std::path::Path, tool: &str, json: &str) -> i32 {
    let args: serde_json::Value = match serde_json::from_str(json) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("hermes-beaver call: arguments are not valid JSON: {e}");
            return 2;
        }
    };
    let mut store = match Store::open(store_path).context("opening the store") {
        Ok(s) => s,
        Err(e) => {
            eprintln!("hermes-beaver call: {e:#}");
            return 1;
        }
    };
    match tools::call(&mut store, tool, args) {
        Ok(out) => {
            println!("{out}");
            0
        }
        Err(e) => {
            eprintln!("hermes-beaver call: {e}");
            match e {
                ToolError::Internal(_) => 1,
                _ => 2,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn parses_call_arguments() {
        let cli = Cli::parse_from(["hermes-beaver", "call", "task_start", r#"{"title":"x"}"#]);
        match cli.command {
            Some(Command::Call { tool, json }) => {
                assert_eq!(tool, "task_start");
                assert_eq!(json, r#"{"title":"x"}"#);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn call_exit_codes() {
        let dir = std::env::temp_dir().join(format!("hb-call-{}", std::process::id()));
        let db = dir.join("events.db");
        assert_eq!(
            run_call(&db, "task_start", r#"{"source":"direct","title":"x"}"#),
            0
        );
        assert_eq!(
            run_call(&db, "task_start", r#"{"source":"nope","title":"x"}"#),
            2
        );
        assert_eq!(run_call(&db, "no_such_tool", "{}"), 2);
        assert_eq!(run_call(&db, "task_start", "{oops"), 2);
        std::fs::remove_dir_all(dir).ok();
    }
}
