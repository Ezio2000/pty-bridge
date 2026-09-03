use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::APP_NAME;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackgroundTaskTicket {
    pub instance_id: String,
    pub session_id: String,
    pub port: u16,
    pub token: String,
    pub expires_at_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Ownership {
    pub host_session_id: String,
    pub instance_id: String,
    pub session_id: String,
    pub port: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlRecord {
    pub instance_id: String,
    pub session_id: String,
    pub port: u16,
    pub token: String,
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

pub fn runtime_root() -> Result<PathBuf> {
    #[cfg(unix)]
    let path = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let uid = unsafe { libc::getuid() };
            std::env::temp_dir().join(format!("{APP_NAME}-{uid}"))
        });

    #[cfg(windows)]
    let path = dirs::data_local_dir()
        .context("unable to locate LocalAppData")?
        .join(APP_NAME)
        .join("runtime");

    fs::create_dir_all(&path).context("create runtime directory")?;
    set_private_dir(&path)?;
    Ok(path)
}

pub fn instance_dir(instance_id: &str) -> Result<PathBuf> {
    validate_id(instance_id)?;
    let path = runtime_root()?.join(instance_id);
    fs::create_dir_all(&path)?;
    set_private_dir(&path)?;
    Ok(path)
}

pub fn background_task_ticket_path(instance_id: &str, session_id: &str) -> Result<PathBuf> {
    validate_id(session_id)?;
    Ok(instance_dir(instance_id)?.join(format!("{session_id}.bgtask")))
}

pub fn write_background_task_ticket(ticket: &BackgroundTaskTicket) -> Result<PathBuf> {
    let path = background_task_ticket_path(&ticket.instance_id, &ticket.session_id)?;
    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts
        .open(&path)
        .context("create private background task ticket")?;
    serde_json::to_writer(&mut file, ticket)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    Ok(path)
}

pub fn read_background_task_ticket(
    instance_id: &str,
    session_id: &str,
) -> Result<BackgroundTaskTicket> {
    let path = background_task_ticket_path(instance_id, session_id)?;
    let data = fs::read(&path).context("read background task ticket")?;
    let ticket: BackgroundTaskTicket =
        serde_json::from_slice(&data).context("parse background task ticket")?;
    if ticket.expires_at_ms < now_ms() {
        let _ = fs::remove_file(&path);
        bail!("background task ticket expired");
    }
    Ok(ticket)
}

pub fn remove_background_task_ticket(instance_id: &str, session_id: &str) {
    if let Ok(path) = background_task_ticket_path(instance_id, session_id) {
        let _ = fs::remove_file(path);
    }
}

pub fn write_control(record: &ControlRecord) -> Result<()> {
    let path = instance_dir(&record.instance_id)?.join(format!("{}.control", record.session_id));
    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts.open(path)?;
    serde_json::to_writer(&mut file, record)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    Ok(())
}

pub fn read_control(instance_id: &str, session_id: &str) -> Result<ControlRecord> {
    validate_id(session_id)?;
    let path = instance_dir(instance_id)?.join(format!("{session_id}.control"));
    serde_json::from_slice(&fs::read(path)?).context("parse control record")
}

pub fn remove_control(instance_id: &str, session_id: &str) {
    if let Ok(path) = instance_dir(instance_id) {
        let _ = fs::remove_file(path.join(format!("{session_id}.control")));
    }
}

pub fn ownership_dir(host_session_id: &str) -> Result<PathBuf> {
    validate_id(host_session_id)?;
    let path = runtime_root()?.join("owners").join(host_session_id);
    fs::create_dir_all(&path)?;
    set_private_dir(&path)?;
    Ok(path)
}

pub fn write_ownership(record: &Ownership) -> Result<()> {
    let path = ownership_dir(&record.host_session_id)?.join(format!(
        "{}--{}.json",
        record.instance_id, record.session_id
    ));
    let mut opts = OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts.open(path)?;
    serde_json::to_writer(&mut file, record)?;
    file.write_all(b"\n")?;
    Ok(())
}

pub fn read_ownership(host_session_id: &str) -> Result<Vec<Ownership>> {
    let dir = ownership_dir(host_session_id)?;
    let mut records = Vec::new();
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.extension().and_then(|v| v.to_str()) != Some("json") {
            continue;
        }
        if let Ok(data) = fs::read(&path)
            && let Ok(record) = serde_json::from_slice(&data)
        {
            records.push(record);
        }
    }
    Ok(records)
}

pub fn remove_ownership(host_session_id: &str) {
    if let Ok(path) = ownership_dir(host_session_id) {
        let _ = fs::remove_dir_all(path);
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

fn set_private_dir(_path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(_path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}
