use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::Serialize;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::TcpStream,
};

use crate::{
    manager::{FinishReason, SessionEvent},
    runtime,
};

#[derive(Serialize)]
struct AttachRequest<'a> {
    action: &'static str,
    instance_id: &'a str,
    session_id: &'a str,
    token: &'a str,
}

pub async fn run(instance_id: &str, session_id: &str) -> Result<()> {
    let ticket = runtime::read_background_task_ticket(instance_id, session_id)?;
    let mut stream = tokio::time::timeout(
        Duration::from_secs(5),
        TcpStream::connect(("127.0.0.1", ticket.port)),
    )
    .await
    .context("connect to local PTY service timeout")??;
    let request = serde_json::to_vec(&AttachRequest {
        action: "attach_background_task",
        instance_id,
        session_id,
        token: &ticket.token,
    })?;
    stream.write_all(&request).await?;
    stream.write_all(b"\n").await?;

    let mut lines = BufReader::new(stream).lines();
    let mut last_output = Instant::now() - Duration::from_secs(2);
    while let Some(line) = lines.next_line().await? {
        let event: SessionEvent =
            serde_json::from_str(&line).context("parse PTY lifecycle event")?;
        match event {
            SessionEvent::Attached => println!("[{session_id}] attached"),
            SessionEvent::Running { program } => {
                println!("[{session_id}] running {program}")
            }
            SessionEvent::Output { preview } => {
                if last_output.elapsed() >= Duration::from_secs(1) {
                    println!("[{session_id}] {preview}");
                    last_output = Instant::now();
                }
            }
            SessionEvent::Finished { termination } => {
                println!(
                    "[{session_id}] finished reason={:?} exit_code={:?}",
                    termination.reason, termination.exit_code
                );
                return match termination.reason {
                    FinishReason::NaturalExit if termination.exit_code == Some(0) => Ok(()),
                    FinishReason::NaturalExit => {
                        bail!("PTY process exited with code {:?}", termination.exit_code)
                    }
                    FinishReason::StartFailed | FinishReason::TicketExpired => {
                        bail!(
                            "PTY failed: {}",
                            termination.message.as_deref().unwrap_or("unknown failure")
                        )
                    }
                    FinishReason::ExplicitClose
                    | FinishReason::BackgroundTaskDisconnected
                    | FinishReason::HostSessionEnded
                    | FinishReason::ServerShutdown
                    | FinishReason::Terminated
                    | FinishReason::Killed => Ok(()),
                };
            }
        }
    }
    bail!("PTY service disconnected before a terminal lifecycle event")
}
