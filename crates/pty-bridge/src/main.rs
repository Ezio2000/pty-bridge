use anyhow::Result;
use clap::{Parser, Subcommand};
use rmcp::{ServiceExt, transport::stdio};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "pty-bridge", version, about = "Cross-platform PTY bridge")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Mcp,
    BackgroundTask {
        #[arg(long)]
        instance: String,
        #[arg(long)]
        session: String,
    },
    Hook {
        #[command(subcommand)]
        command: HookCommand,
    },
}

#[derive(Subcommand)]
enum HookCommand {
    Bind,
    Cleanup,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();
    match Cli::parse().command {
        Command::Mcp => {
            let server = pty_bridge::mcp::PtyServer::new().await?;
            server.serve(stdio()).await?.waiting().await?;
        }
        Command::BackgroundTask { instance, session } => {
            pty_bridge::background_task::run(&instance, &session).await?
        }
        Command::Hook {
            command: HookCommand::Bind,
        } => pty_bridge::hooks::bind_from_stdin().await?,
        Command::Hook {
            command: HookCommand::Cleanup,
        } => pty_bridge::hooks::cleanup_from_stdin().await?,
    }
    Ok(())
}
