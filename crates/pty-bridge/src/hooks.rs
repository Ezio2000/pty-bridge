use std::io::Read;

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use serde_json::Value;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::TcpStream,
};

use crate::runtime::{self, Ownership};

#[derive(Debug, Deserialize)]
struct HookInput {
    session_id: String,
    #[serde(default)]
    tool_response: Value,
}

pub async fn bind_from_stdin() -> Result<()> {
    let input = read_hook_input()?;
    let response = parse_response(input.tool_response)?;
    let record = Ownership {
        host_session_id: input.session_id,
        instance_id: string_field(&response, "instance_id")?,
        session_id: string_field(&response, "session_id")?,
        port: number_field(&response, "control_port")? as u16,
    };
    runtime::write_ownership(&record)
}

pub async fn cleanup_from_stdin() -> Result<()> {
    let input = read_hook_input()?;
    let mut errors = Vec::new();
    for record in runtime::read_ownership(&input.session_id)? {
        if let Ok(control) = runtime::read_control(&record.instance_id, &record.session_id)
            && let Err(error) = send_close(&record, &control.token).await
        {
            errors.push(format!("{}: {error}", record.session_id));
        }
        runtime::remove_background_task_ticket(&record.instance_id, &record.session_id);
    }
    if errors.is_empty() {
        runtime::remove_ownership(&input.session_id);
        Ok(())
    } else {
        bail!("failed to close PTY sessions: {}", errors.join(", "))
    }
}

async fn send_close(record: &Ownership, token: &str) -> Result<()> {
    let mut stream = BufReader::new(TcpStream::connect(("127.0.0.1", record.port)).await?);
    let message = serde_json::json!({
        "action": "cleanup",
        "instance_id": record.instance_id,
        "session_id": record.session_id,
        "token": token,
    });
    stream
        .get_mut()
        .write_all(serde_json::to_string(&message)?.as_bytes())
        .await?;
    stream.get_mut().write_all(b"\n").await?;
    let mut response = String::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        stream.read_line(&mut response),
    )
    .await
    .context("cleanup acknowledgement timeout")??;
    if response.trim() != r#"{"ok":true}"# {
        bail!("cleanup was not acknowledged");
    }
    Ok(())
}

fn read_hook_input() -> Result<HookInput> {
    let mut data = String::new();
    std::io::stdin().read_to_string(&mut data)?;
    serde_json::from_str(&data).context("parse hook input")
}

fn parse_response(mut value: Value) -> Result<Value> {
    for _ in 0..3 {
        if let Some(structured) = value.get("structuredContent") {
            value = structured.clone();
            continue;
        }
        if let Some(encoded) = value.as_str() {
            value = serde_json::from_str(encoded).context("parse MCP tool response JSON")?;
            continue;
        }
        if value.is_object() {
            return Ok(value);
        }
        bail!("unsupported MCP tool response shape");
    }
    bail!("MCP tool response is nested too deeply")
}

fn string_field(value: &Value, name: &str) -> Result<String> {
    value
        .get(name)
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .ok_or_else(|| anyhow!("missing {name} in tool response"))
}

fn number_field(value: &Value, name: &str) -> Result<u64> {
    value
        .get(name)
        .and_then(Value::as_u64)
        .ok_or_else(|| anyhow!("missing {name} in tool response"))
}

#[cfg(test)]
mod tests {
    use super::parse_response;

    #[test]
    fn parses_direct_mcp_response() {
        let value = serde_json::json!({"session_id": "pty_1"});
        assert_eq!(parse_response(value).unwrap()["session_id"], "pty_1");
    }

    #[test]
    fn parses_structured_mcp_response() {
        let value = serde_json::json!({
            "structuredContent": {"session_id": "pty_2"}
        });
        assert_eq!(parse_response(value).unwrap()["session_id"], "pty_2");
    }

    #[test]
    fn parses_json_string_used_by_host_hooks() {
        let value = serde_json::json!(r#"{"session_id":"pty_3"}"#);
        assert_eq!(parse_response(value).unwrap()["session_id"], "pty_3");
    }
}
