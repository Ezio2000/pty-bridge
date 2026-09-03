use std::{io::Read, time::Duration};

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use serde_json::Value;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::TcpStream,
    task::JoinSet,
};

use crate::runtime::{self, Ownership, OwnershipEntry, ProcessLocator};

const CONTROL_TIMEOUT: Duration = Duration::from_millis(250);
const CLEANUP_DEADLINE: Duration = Duration::from_millis(750);

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
        port: port_field(&response, "control_port")?,
    };
    runtime::write_ownership(&record)?;
    async {
        let control = runtime::read_control(&record.instance_id, &record.session_id)?;
        send_control(
            record.port,
            serde_json::json!({
                "action": "bind_owner",
                "instance_id": record.instance_id,
                "session_id": record.session_id,
                "host_session_id": record.host_session_id,
                "token": control.token,
            }),
        )
        .await
    }
    .await
    .context("bind PTY ownership")
}

pub async fn cleanup_from_stdin() -> Result<()> {
    let input = read_hook_input()?;
    let scan = runtime::read_ownership(&input.session_id)?;
    let mut tasks = JoinSet::new();
    for entry in scan.entries {
        tasks.spawn(cleanup_entry(entry));
    }

    let cleanup = async {
        let mut errors = scan.errors;
        while let Some(result) = tasks.join_next().await {
            match result {
                Ok(Ok(())) => {}
                Ok(Err(error)) => errors.push(format!("{error:#}")),
                Err(error) => errors.push(error.to_string()),
            }
        }
        errors
    };
    let errors = match tokio::time::timeout(CLEANUP_DEADLINE, cleanup).await {
        Ok(errors) => errors,
        Err(_) => {
            tasks.abort_all();
            bail!("PTY cleanup exceeded {} ms", CLEANUP_DEADLINE.as_millis());
        }
    };
    if errors.is_empty() {
        Ok(())
    } else {
        bail!("failed to finish PTY sessions: {}", errors.join(", "))
    }
}

async fn cleanup_entry(entry: OwnershipEntry) -> Result<()> {
    let record = &entry.record;
    let control_result = match runtime::read_control(&record.instance_id, &record.session_id) {
        Ok(control) => send_control(
            record.port,
            serde_json::json!({
                "action": "finish_owned",
                "instance_id": record.instance_id,
                "session_id": record.session_id,
                "token": control.token,
            }),
        )
        .await
        .with_context(|| format!("finish {}", record.session_id)),
        Err(error) => Err(error).with_context(|| format!("read control for {}", record.session_id)),
    };
    if let Err(control_error) = control_result {
        let locator = runtime::read_process_locator(&entry)
            .with_context(|| format!("locate process for {}", record.session_id))?;
        if let Some(locator) = locator {
            terminate_process(&locator).with_context(|| {
                format!(
                    "directly finish {} after control failure ({control_error:#})",
                    record.session_id
                )
            })?;
        }
    }
    runtime::remove_session_credentials(&record.instance_id, &record.session_id);
    runtime::remove_ownership_entry(&entry);
    Ok(())
}

#[cfg(unix)]
fn terminate_process(locator: &ProcessLocator) -> Result<()> {
    let ProcessLocator::Unix {
        process_id,
        process_group,
    } = locator
    else {
        bail!("process locator does not match this platform");
    };
    if let Some(group) = process_group {
        if *group <= 0 {
            bail!("invalid process group in ownership record");
        }
        if unsafe { libc::kill(-group, libc::SIGKILL) } == 0 {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        return if error.raw_os_error() == Some(libc::ESRCH) {
            Ok(())
        } else {
            Err(error).context("kill owned process group")
        };
    }
    if *process_id <= 0 {
        bail!("invalid process id in ownership record");
    }
    if unsafe { libc::kill(*process_id, libc::SIGKILL) } == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        Ok(())
    } else {
        Err(error).context("kill owned process")
    }
}

#[cfg(windows)]
fn terminate_process(locator: &ProcessLocator) -> Result<()> {
    use windows_sys::Win32::{
        Foundation::{CloseHandle, ERROR_FILE_NOT_FOUND},
        System::{
            JobObjects::{OpenJobObjectW, TerminateJobObject},
            SystemServices::JOB_OBJECT_TERMINATE,
        },
    };

    let ProcessLocator::WindowsJob { name } = locator else {
        bail!("process locator does not match this platform");
    };
    let wide_name: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    let job = unsafe { OpenJobObjectW(JOB_OBJECT_TERMINATE, 0, wide_name.as_ptr()) };
    if job.is_null() {
        let error = std::io::Error::last_os_error();
        return if error.raw_os_error() == Some(ERROR_FILE_NOT_FOUND as i32) {
            Ok(())
        } else {
            Err(error).context("open owned job object")
        };
    }
    let terminated = unsafe { TerminateJobObject(job, 1) };
    unsafe { CloseHandle(job) };
    if terminated == 0 {
        Err(std::io::Error::last_os_error()).context("terminate owned job object")
    } else {
        Ok(())
    }
}

async fn send_control(port: u16, message: Value) -> Result<()> {
    tokio::time::timeout(CONTROL_TIMEOUT, async {
        let mut stream = BufReader::new(TcpStream::connect(("127.0.0.1", port)).await?);
        stream
            .get_mut()
            .write_all(serde_json::to_string(&message)?.as_bytes())
            .await?;
        stream.get_mut().write_all(b"\n").await?;
        let mut response = String::new();
        stream.read_line(&mut response).await?;
        let response: Value = serde_json::from_str(response.trim())?;
        if response.get("ok").and_then(Value::as_bool) != Some(true) {
            bail!("control request was not acknowledged");
        }
        Result::<()>::Ok(())
    })
    .await
    .context("control request timeout")?
}

fn read_hook_input() -> Result<HookInput> {
    let mut data = String::new();
    std::io::stdin().read_to_string(&mut data)?;
    serde_json::from_str(&data).context("parse hook input")
}

fn parse_response(mut value: Value) -> Result<Value> {
    for _ in 0..4 {
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
        break;
    }
    Err(anyhow!("unsupported MCP tool response shape"))
}

fn string_field(value: &Value, key: &str) -> Result<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("missing {key} in MCP tool response"))
}

fn port_field(value: &Value, key: &str) -> Result<u16> {
    let port = value
        .get(key)
        .and_then(Value::as_u64)
        .ok_or_else(|| anyhow!("missing {key} in MCP tool response"))?;
    u16::try_from(port)
        .ok()
        .filter(|port| *port != 0)
        .ok_or_else(|| anyhow!("invalid {key} in MCP tool response"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_serialized_structured_response() {
        let value = Value::String(
            serde_json::json!({
                "structuredContent": {
                    "instance_id": "inst_1",
                    "session_id": "pty_1",
                    "control_port": 1234
                }
            })
            .to_string(),
        );
        let parsed = parse_response(value).unwrap();
        assert_eq!(string_field(&parsed, "session_id").unwrap(), "pty_1");
        assert_eq!(port_field(&parsed, "control_port").unwrap(), 1234);
    }

    #[test]
    fn rejects_invalid_ports() {
        let value = serde_json::json!({"control_port": 70000});
        assert!(port_field(&value, "control_port").is_err());
    }
}
