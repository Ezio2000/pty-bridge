use crate::{
    protocol::{receive, send},
    runtime,
};
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{io::Read, time::Duration};
use tokio::{io::BufReader, net::TcpStream, task::JoinSet};

#[derive(Deserialize)]
struct HookInput {
    session_id: String,
    #[serde(default)]
    tool_input: Value,
    #[serde(default)]
    tool_response: Value,
}
fn input() -> Result<HookInput> {
    let mut data = String::new();
    std::io::stdin().read_to_string(&mut data)?;
    Ok(serde_json::from_str(&data)?)
}

pub fn prepare_from_stdin() -> Result<()> {
    let input = input()?;
    runtime::validate_id(&input.session_id)?;
    let mut arguments = input
        .tool_input
        .as_object()
        .context("start input must be an object")?
        .clone();
    arguments.insert("host_session_id".into(), json!(input.session_id));
    println!(
        "{}",
        json!({"hookSpecificOutput":{"hookEventName":"PreToolUse","updatedInput":arguments}})
    );
    Ok(())
}
pub async fn observe_from_stdin() -> Result<()> {
    let input = input()?;
    let response = parse_response(input.tool_response)?;
    let Some(receipt) = response.get("receipt").and_then(Value::as_str) else {
        return Ok(());
    };
    let session = input.tool_input["session_id"]
        .as_str()
        .context("read input has no session_id")?;
    // A finished session has no ownership record and no pending silence to acknowledge.
    let Some(record) = runtime::read_ownership(&input.session_id)?
        .into_iter()
        .find(|record| record.session_id == session)
    else {
        return Ok(());
    };
    control(record.port,json!({"action":"observe","instance_id":record.instance_id,"session_id":session,"receipt_id":receipt})).await
}

pub async fn cleanup_from_stdin() -> Result<()> {
    let input = input()?;
    let records = runtime::read_ownership(&input.session_id)?;
    let mut tasks = JoinSet::new();
    for record in records {
        tasks.spawn(async move {
        if control(record.port,json!({"action":"finish_owned","instance_id":record.instance_id,"session_id":record.session_id})).await.is_err() {
            pty_core::platform::terminate_tree(&record.process)?;
        }
        runtime::remove_ownership(&record);Ok::<_,anyhow::Error>(())
    });
    }
    tokio::time::timeout(Duration::from_millis(750), async {
        let mut errors = vec![];
        while let Some(result) = tasks.join_next().await {
            match result {
                Ok(Ok(())) => {}
                Ok(Err(e)) => errors.push(e.to_string()),
                Err(e) => errors.push(e.to_string()),
            }
        }
        if !errors.is_empty() {
            bail!("PTY cleanup failed: {}", errors.join(", "));
        }
        Ok::<_, anyhow::Error>(())
    })
    .await
    .context("PTY cleanup exceeded shutdown deadline")??;
    Ok(())
}

async fn control(port: u16, message: Value) -> Result<()> {
    tokio::time::timeout(Duration::from_millis(250), async {
        let mut stream = BufReader::new(TcpStream::connect(("127.0.0.1", port)).await?);
        send(stream.get_mut(), &message).await?;
        let result: Value = receive(&mut stream).await?;
        if result["ok"] != true {
            bail!("control request rejected: {}", result["error"]);
        }
        Ok::<_, anyhow::Error>(())
    })
    .await
    .context("control request timeout")?
}
fn parse_response(mut value: Value) -> Result<Value> {
    for _ in 0..5 {
        if let Some(structured) = value.get("structuredContent") {
            value = structured.clone();
            continue;
        }
        if let Some(encoded) = value.as_str() {
            value = serde_json::from_str(encoded)?;
            continue;
        }
        if let Some(content) = value.get("content").and_then(Value::as_array)
            && let Some(text) = content
                .iter()
                .find_map(|v| v.get("text").and_then(Value::as_str))
        {
            value = serde_json::from_str(text)?;
            continue;
        }
        if value.is_object() {
            return Ok(value);
        }
        break;
    }
    bail!("unsupported MCP response shape")
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reads_actual_structured_and_text_mcp_results() {
        let result = json!({"receipt":"r1","next_cursor":5});
        for response in [
            json!({"structuredContent":result}).to_string().into(),
            json!({"content":[{"type":"text","text":result.to_string()}]}),
        ] {
            assert_eq!(parse_response(response).unwrap(), result);
        }
    }
}
