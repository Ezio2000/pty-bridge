use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::Serialize;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::TcpStream,
};

use crate::runtime;

#[derive(Serialize)]
struct WatchRequest<'a> {
    instance_id: &'a str,
    session_id: &'a str,
    token: &'a str,
}

pub async fn run(instance_id: &str, session_id: &str) -> Result<()> {
    let ticket = runtime::read_ticket(instance_id, session_id)?;
    if ticket.instance_id != instance_id || ticket.session_id != session_id {
        bail!("watch ticket identity mismatch");
    }
    let mut stream = TcpStream::connect(("127.0.0.1", ticket.port))
        .await
        .context("connect to local PTY service")?;
    let request = serde_json::to_vec(&WatchRequest {
        instance_id,
        session_id,
        token: &ticket.token,
    })?;
    stream.write_all(&request).await?;
    stream.write_all(b"\n").await?;

    let mut lines = BufReader::new(stream).lines();
    let mut last_output = Instant::now() - Duration::from_secs(2);
    while let Some(line) = lines.next_line().await? {
        if line.starts_with("[output]") {
            if last_output.elapsed() < Duration::from_secs(1) {
                continue;
            }
            last_output = Instant::now();
        }
        println!("[{session_id}] {line}");
    }
    Ok(())
}
