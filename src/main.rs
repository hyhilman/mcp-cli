#![forbid(unsafe_code)]

//! mcp-cli: expose a fixed set of CLI commands as MCP tools.

mod config;
mod events;
mod mcp;
mod notify;
mod runner;
mod state;
mod transport;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use clap::{Parser, Subcommand};

use crate::config::{tool_names_of, Config};
use crate::events::{EventHub, StateChange};
use crate::mcp::session::ToolCallParams;
use crate::mcp::McpSession;
use crate::state::{CliState, LoginOutcome, StateEvent, StateManager};

#[derive(Parser)]
#[command(
    name = "mcp-cli",
    version,
    about = "Expose a fixed set of CLI commands as MCP tools"
)]
struct Args {
    /// Config file [default: $MCP_CLI_CONFIG or ~/.config/mcp-cli/config.toml]
    #[arg(short, long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the MCP server (stdio by default)
    Serve {
        /// Serve Streamable HTTP on this address instead of stdio, e.g. 127.0.0.1:8090.
        /// Non-loopback addresses require MCP_CLI_TOKEN.
        #[arg(long)]
        http: Option<SocketAddr>,
    },
    /// Validate the config and list the tools it defines
    Check,
    /// Run health checks and print each CLI's state
    Status { name: Option<String> },
    /// Start a CLI's login, print the link and wait for sign-in
    Login { name: String },
    /// Call one tool, e.g. `mcp-cli call aws_list_instances region=us-east-1`
    Call {
        tool: String,
        /// Arguments as key=value
        args: Vec<String>,
    },
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    let command = args.command.unwrap_or(Command::Serve { http: None });
    // Logs go to stderr: stdout is the MCP channel in stdio mode.
    let default_level = if matches!(command, Command::Serve { .. }) {
        "info"
    } else {
        "warn"
    };
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("MCP_CLI_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default_level)),
        )
        .init();

    if let Err(e) = run(args.config, command).await {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

async fn run(config_path: Option<PathBuf>, command: Command) -> Result<(), Box<dyn std::error::Error>> {
    let path = config_path.unwrap_or_else(config::default_config_path);
    let config = Arc::new(Config::load(&path)?);
    let states = StateManager::new(config.clis.iter().filter_map(|c| c.state.clone()).collect());

    match command {
        Command::Serve { http } => serve(config, states, http).await,
        Command::Check => {
            println!("{}: OK", path.display());
            for cli in &config.clis {
                println!("  {} -> {}", cli.name, cli.program.bin.display());
                for tool in tool_names_of(cli) {
                    println!("    {tool}");
                }
            }
            Ok(())
        }
        Command::Status { name } => {
            let names = name.map(|n| vec![n]).unwrap_or_else(|| states.names());
            if names.is_empty() {
                println!("No CLI has a [health] check.");
            }
            for n in names {
                let s = states.refresh(&n).await?;
                println!(
                    "  {:<16} {:<12} expires={} {}",
                    s.name,
                    s.state.to_string(),
                    s.expires_at.as_deref().unwrap_or("-"),
                    s.detail.as_deref().unwrap_or(""),
                );
            }
            Ok(())
        }
        Command::Login { name } => login(&states, &name).await,
        Command::Call { tool, args } => {
            let session = McpSession::new(config, states.clone());
            for n in states.names() {
                states.refresh(&n).await?;
            }
            let params = ToolCallParams {
                name: tool,
                arguments: Some(parse_kv(&args)?),
                request_state: None,
                input_responses: None,
            };
            let result = session.call_tool_direct(&params).await;
            for item in result["content"].as_array().into_iter().flatten() {
                println!("{}", item["text"].as_str().unwrap_or_default());
            }
            if result["isError"] == true {
                std::process::exit(1);
            }
            Ok(())
        }
    }
}

async fn serve(
    config: Arc<Config>,
    states: Arc<StateManager>,
    http: Option<SocketAddr>,
) -> Result<(), Box<dyn std::error::Error>> {
    states.start_watchers();
    let auto_login: Vec<String> = config
        .clis
        .iter()
        .filter(|c| c.auto_login)
        .map(|c| c.name.clone())
        .collect();
    notify::spawn_notifier(states.clone(), config.notify.clone(), auto_login);

    let mut session = McpSession::new(config.clone(), states.clone());
    if let Some(events) = &config.events {
        if let Some(dir) = events.store_path.as_ref().and_then(|p| p.parent()) {
            std::fs::create_dir_all(dir)?;
        }
        let hub = EventHub::new(events.clone(), states.names());
        spawn_event_publisher(&states, hub.clone());
        session = session.with_events(hub);
    }
    let session = Arc::new(session);

    match http {
        Some(addr) => {
            let token = std::env::var("MCP_CLI_TOKEN").ok().filter(|t| !t.is_empty());
            if token.is_none() {
                tracing::warn!("MCP_CLI_TOKEN is not set; HTTP requests are not authenticated");
            }
            transport::http::run_http(session, addr, token).await?
        }
        None => transport::stdio::run_stdio(session).await?,
    }
    Ok(())
}

/// Publish state changes as MCP Events.
fn spawn_event_publisher(states: &Arc<StateManager>, hub: Arc<EventHub>) {
    let mut events = states.subscribe();
    let states = states.clone();
    tokio::spawn(async move {
        loop {
            let StateEvent::StateChanged { name, from, to } = match events.recv().await {
                Ok(event) => event,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            };
            if from == CliState::Unknown && to == CliState::Ready {
                continue;
            }
            if to == CliState::NeedsLogin {
                // Give an auto login (if configured) a moment to produce the link.
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
            let login_url = states.status(&name).and_then(|s| s.login).map(|l| l.url);
            hub.publish(StateChange {
                cli: name,
                from: from.to_string(),
                to: to.to_string(),
                login_url,
            });
        }
    });
}

async fn login(states: &Arc<StateManager>, name: &str) -> Result<(), Box<dyn std::error::Error>> {
    match states.start_login(name).await? {
        LoginOutcome::AlreadyReady { .. } => println!("{name} is already ready."),
        LoginOutcome::Pending { login } => {
            println!(
                "\n=== Sign in to {name} ===\n  {}\n\n{}\n",
                login.url, login.message
            );
            if states.supports_paste_back(name) {
                println!("If the browser ends on a http://localhost:... page that fails to load,");
                println!("paste that full address here and press Enter.");
                spawn_paste_back_reader(states.clone(), name.to_string());
            }
            println!("Waiting for sign-in...");
            // The background task clears `login` once the command exits or times out.
            while states.status(name).is_some_and(|s| s.login.is_some()) {
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            let s = states.refresh(name).await?;
            if s.state != CliState::Ready {
                return Err(format!("{name} is still {}", s.state).into());
            }
            println!("{name} is ready.");
        }
    }
    Ok(())
}

/// Read pasted localhost callback addresses from stdin during `login`.
fn spawn_paste_back_reader(states: Arc<StateManager>, name: String) {
    tokio::spawn(async move {
        use tokio::io::AsyncBufReadExt;
        let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            match states.complete_login(&name, line).await {
                Ok(status) => println!("Callback delivered ({status})."),
                Err(e) => println!("error: {e}"),
            }
        }
    });
}

fn parse_kv(args: &[String]) -> Result<serde_json::Value, String> {
    let mut map = serde_json::Map::new();
    for arg in args {
        let (k, v) = arg
            .split_once('=')
            .ok_or_else(|| format!("expected key=value, got '{arg}'"))?;
        map.insert(k.to_string(), v.into());
    }
    Ok(serde_json::Value::Object(map))
}
