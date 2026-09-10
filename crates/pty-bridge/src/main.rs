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
    Monitor {
        #[arg(long, env = "CLAUDE_CODE_SESSION_ID")]
        host_session_id: String,
    },
    Wait {
        #[arg(long)]
        instance: String,
        #[arg(long)]
        session: String,
        #[arg(long)]
        port: u16,
    },
    Hook {
        #[command(subcommand)]
        command: HookCommand,
    },
}
#[derive(Subcommand)]
enum HookCommand {
    Prepare,
    Observe,
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
            let manager = server.manager.clone();
            let result = async {
                server.serve(stdio()).await?.waiting().await?;
                Ok::<_, anyhow::Error>(())
            }
            .await;
            manager.shutdown_and_wait().await?;
            result?;
        }
        Command::Monitor { host_session_id } => pty_bridge::monitor::run(&host_session_id).await?,
        Command::Wait {
            instance,
            session,
            port,
        } => {
            let code = pty_bridge::wait::run(&instance, &session, port).await?;
            if code != 0 {
                std::process::exit(code);
            }
        }
        Command::Hook {
            command: HookCommand::Prepare,
        } => pty_bridge::hooks::prepare_from_stdin()?,
        Command::Hook {
            command: HookCommand::Observe,
        } => pty_bridge::hooks::observe_from_stdin().await?,
        Command::Hook {
            command: HookCommand::Cleanup,
        } => pty_bridge::hooks::cleanup_from_stdin().await?,
    }
    Ok(())
}
