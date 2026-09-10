use crate::protocol::{receive, send};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use tokio::{io::BufReader, net::TcpStream};

pub async fn run(instance: &str, session: &str, port: u16) -> Result<i32> {
    let mut stream = BufReader::new(
        TcpStream::connect(("127.0.0.1", port))
            .await
            .context("connect to PTY owner")?,
    );
    send(
        stream.get_mut(),
        &json!({"action":"wait","instance_id":instance,"session_id":session}),
    )
    .await?;
    let ack: Value = receive(&mut stream).await?;
    if ack["ok"] != true {
        bail!("bgshell registration rejected: {}", ack["error"]);
    }
    let result: Value = receive(&mut stream).await?;
    let terminal = &result["termination"];
    let reason = terminal["reason"]
        .as_str()
        .context("owner disconnected without a terminal result")?;
    println!(
        "[{session}] finished reason={reason} exit_code={}{}",
        terminal["exit_code"],
        terminal["message"]
            .as_str()
            .map(|s| format!(" message={s}"))
            .unwrap_or_default()
    );
    Ok(match reason {
        "natural_exit" => {
            if terminal["exit_code"] == 0 {
                0
            } else {
                1
            }
        }
        "explicit_close" | "terminated" | "killed" | "host_session_ended" => 0,
        _ => 1,
    })
}
