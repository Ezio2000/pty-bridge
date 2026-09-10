use anyhow::{Context, Result, bail};
use pty_core::platform::ProcessLocator;
use serde::{Deserialize, Serialize};
use std::{fs, io::Write, path::PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MonitorRecord {
    pub host_session_id: String,
    pub generation: String,
    pub port: u16,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Ownership {
    pub host_session_id: String,
    pub instance_id: String,
    pub session_id: String,
    pub port: u16,
    pub process: ProcessLocator,
}

pub fn root() -> Result<PathBuf> {
    #[cfg(unix)]
    {
        if let Some(base) = std::env::var_os("XDG_RUNTIME_DIR") {
            let base = PathBuf::from(base);
            if base.is_absolute() {
                return Ok(base.join("pty-bridge"));
            }
        }
        Ok(std::env::temp_dir().join(format!("pty-bridge-{}", unsafe { libc::getuid() })))
    }
    #[cfg(windows)]
    {
        Ok(dirs::data_local_dir()
            .context("unable to locate LocalAppData")?
            .join("pty-bridge")
            .join("runtime"))
    }
}

pub fn validate_id(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        bail!("invalid identifier");
    }
    Ok(())
}

pub fn monitor_path(host: &str) -> Result<PathBuf> {
    validate_id(host)?;
    Ok(root()?.join("monitors").join(format!("{host}.json")))
}
pub fn read_monitor(host: &str) -> Result<MonitorRecord> {
    let record: MonitorRecord =
        serde_json::from_slice(&fs::read(monitor_path(host)?).context("monitor is not ready")?)?;
    if record.host_session_id != host || record.port == 0 {
        bail!("invalid monitor record");
    }
    Ok(record)
}

pub fn ownership_path(host: &str, instance: &str, session: &str) -> Result<PathBuf> {
    validate_id(host)?;
    validate_id(instance)?;
    validate_id(session)?;
    Ok(root()?
        .join("owners")
        .join(host)
        .join(format!("{instance}--{session}.json")))
}

pub fn write_ownership(record: &Ownership) -> Result<()> {
    let path = ownership_path(
        &record.host_session_id,
        &record.instance_id,
        &record.session_id,
    )?;
    fs::create_dir_all(path.parent().unwrap())?;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)?;
    if let Err(error) = (|| -> Result<()> {
        serde_json::to_writer(&mut file, record)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        Ok(())
    })() {
        let _ = fs::remove_file(path);
        return Err(error);
    }
    Ok(())
}

pub fn remove_ownership(record: &Ownership) {
    if let Ok(path) = ownership_path(
        &record.host_session_id,
        &record.instance_id,
        &record.session_id,
    ) {
        let _ = fs::remove_file(&path);
        if let Some(parent) = path.parent() {
            let _ = fs::remove_dir(parent);
        }
    }
}

pub fn read_ownership(host: &str) -> Result<Vec<Ownership>> {
    validate_id(host)?;
    let directory = root()?.join("owners").join(host);
    let files = match fs::read_dir(directory) {
        Ok(files) => files,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return Err(e.into()),
    };
    let mut result = Vec::new();
    for file in files {
        let path = file?.path();
        if path.extension().is_some_and(|v| v == "json") {
            // Natural finalization may remove a record while the hook scans it.
            let bytes = match fs::read(path) {
                Ok(bytes) => bytes,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e.into()),
            };
            let record: Ownership = serde_json::from_slice(&bytes)?;
            if record.host_session_id != host {
                bail!("owner record mismatch");
            }
            result.push(record);
        }
    }
    Ok(result)
}
