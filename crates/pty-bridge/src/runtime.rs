use std::{
    fs::{self, OpenOptions},
    io::{ErrorKind, Write},
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProcessLocator {
    Unix {
        process_id: i32,
        process_group: Option<i32>,
    },
    WindowsJob {
        name: String,
    },
}

#[derive(Debug, Clone)]
pub struct OwnershipEntry {
    pub record: Ownership,
    pub path: PathBuf,
}

#[derive(Debug, Default)]
pub struct OwnershipScan {
    pub entries: Vec<OwnershipEntry>,
    pub errors: Vec<String>,
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

pub fn runtime_root_path() -> Result<PathBuf> {
    #[cfg(unix)]
    {
        if let Some(base) = std::env::var_os("XDG_RUNTIME_DIR") {
            let base = PathBuf::from(base);
            if base.is_absolute() {
                return Ok(base.join(APP_NAME));
            }
        }
        let uid = unsafe { libc::getuid() };
        Ok(std::env::temp_dir().join(format!("{APP_NAME}-{uid}")))
    }

    #[cfg(windows)]
    {
        Ok(dirs::data_local_dir()
            .context("unable to locate LocalAppData")?
            .join(APP_NAME)
            .join("runtime"))
    }
}

pub fn runtime_root() -> Result<PathBuf> {
    let path = runtime_root_path()?;
    ensure_private_dir(&path)?;
    Ok(path)
}

fn instance_path(instance_id: &str) -> Result<PathBuf> {
    validate_id(instance_id)?;
    Ok(runtime_root_path()?.join(instance_id))
}

fn ensure_instance(instance_id: &str) -> Result<PathBuf> {
    let path = instance_path(instance_id)?;
    ensure_private_dir(&path)?;
    Ok(path)
}

pub fn remove_instance(instance_id: &str) {
    if let Ok(path) = instance_path(instance_id) {
        let _ = fs::remove_dir_all(path);
    }
}

pub fn background_task_ticket_path(instance_id: &str, session_id: &str) -> Result<PathBuf> {
    validate_id(session_id)?;
    Ok(instance_path(instance_id)?.join(format!("{session_id}.bgtask")))
}

pub fn write_background_task_ticket(ticket: &BackgroundTaskTicket) -> Result<PathBuf> {
    validate_id(&ticket.session_id)?;
    let path = ensure_instance(&ticket.instance_id)?.join(format!("{}.bgtask", ticket.session_id));
    write_private_json_new(&path, ticket).context("create private background task ticket")?;
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
    if ticket.instance_id != instance_id || ticket.session_id != session_id {
        bail!("background task ticket identity mismatch");
    }
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

fn control_path(instance_id: &str, session_id: &str) -> Result<PathBuf> {
    validate_id(session_id)?;
    Ok(instance_path(instance_id)?.join(format!("{session_id}.control")))
}

pub fn write_control(record: &ControlRecord) -> Result<()> {
    validate_id(&record.session_id)?;
    let path = ensure_instance(&record.instance_id)?.join(format!("{}.control", record.session_id));
    write_private_json_new(&path, record).context("create private control credential")
}

pub fn read_control(instance_id: &str, session_id: &str) -> Result<ControlRecord> {
    let path = control_path(instance_id, session_id)?;
    let data = fs::read(&path).context("read control credential")?;
    let record: ControlRecord =
        serde_json::from_slice(&data).context("parse control credential")?;
    if record.instance_id != instance_id || record.session_id != session_id {
        bail!("control credential identity mismatch");
    }
    Ok(record)
}

pub fn remove_control(instance_id: &str, session_id: &str) {
    if let Ok(path) = control_path(instance_id, session_id) {
        let _ = fs::remove_file(path);
    }
}

pub fn remove_session_credentials(instance_id: &str, session_id: &str) {
    remove_background_task_ticket(instance_id, session_id);
    remove_control(instance_id, session_id);
    let Ok(path) = instance_path(instance_id) else {
        return;
    };
    let is_empty = fs::read_dir(&path)
        .map(|mut entries| entries.next().is_none())
        .unwrap_or(false);
    if is_empty {
        let _ = fs::remove_dir(path);
    }
}

fn ownership_dir_path(host_session_id: &str) -> Result<PathBuf> {
    validate_id(host_session_id)?;
    Ok(runtime_root_path()?.join("owners").join(host_session_id))
}

fn ownership_path(host_session_id: &str, instance_id: &str, session_id: &str) -> Result<PathBuf> {
    validate_id(instance_id)?;
    validate_id(session_id)?;
    Ok(ownership_dir_path(host_session_id)?.join(format!("{instance_id}--{session_id}.json")))
}

fn process_locator_path(
    host_session_id: &str,
    instance_id: &str,
    session_id: &str,
) -> Result<PathBuf> {
    validate_id(instance_id)?;
    validate_id(session_id)?;
    Ok(ownership_dir_path(host_session_id)?.join(format!("{instance_id}--{session_id}.process")))
}

pub fn write_ownership(record: &Ownership) -> Result<()> {
    validate_id(&record.instance_id)?;
    validate_id(&record.session_id)?;
    let dir = ownership_dir_path(&record.host_session_id)?;
    ensure_private_dir(&dir)?;
    let path = dir.join(format!(
        "{}--{}.json",
        record.instance_id, record.session_id
    ));
    write_private_json_new(&path, record).context("create ownership record")
}

pub fn write_process_locator(
    host_session_id: &str,
    instance_id: &str,
    session_id: &str,
    locator: &ProcessLocator,
) -> Result<()> {
    let path = process_locator_path(host_session_id, instance_id, session_id)?;
    write_private_json_new(&path, locator).context("create private process locator")
}

pub fn read_process_locator(entry: &OwnershipEntry) -> Result<Option<ProcessLocator>> {
    let record = &entry.record;
    let path = process_locator_path(
        &record.host_session_id,
        &record.instance_id,
        &record.session_id,
    )?;
    let data = match fs::read(&path) {
        Ok(data) => data,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("read process locator"),
    };
    serde_json::from_slice(&data)
        .map(Some)
        .context("parse process locator")
}

pub fn read_ownership(host_session_id: &str) -> Result<OwnershipScan> {
    let dir = ownership_dir_path(host_session_id)?;
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(OwnershipScan::default()),
        Err(error) => return Err(error).context("read ownership directory"),
    };
    let mut scan = OwnershipScan::default();
    for entry in entries {
        let path = match entry {
            Ok(entry) => entry.path(),
            Err(error) => {
                scan.errors.push(format!("read ownership entry: {error}"));
                continue;
            }
        };
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        let data = match fs::read(&path) {
            Ok(data) => data,
            Err(error) => {
                scan.errors
                    .push(format!("read ownership record {}: {error}", path.display()));
                continue;
            }
        };
        let record: Ownership = match serde_json::from_slice(&data) {
            Ok(record) => record,
            Err(error) => {
                scan.errors.push(format!(
                    "parse ownership record {}: {error}",
                    path.display()
                ));
                continue;
            }
        };
        if let Err(error) = validate_ownership(&record, host_session_id) {
            scan.errors.push(format!(
                "validate ownership record {}: {error}",
                path.display()
            ));
            continue;
        }
        scan.entries.push(OwnershipEntry { record, path });
    }
    Ok(scan)
}

fn validate_ownership(record: &Ownership, host_session_id: &str) -> Result<()> {
    validate_id(&record.host_session_id)?;
    validate_id(&record.instance_id)?;
    validate_id(&record.session_id)?;
    if record.host_session_id != host_session_id {
        bail!("host session mismatch");
    }
    Ok(())
}

pub fn remove_ownership_record(host_session_id: &str, instance_id: &str, session_id: &str) {
    if let Ok(path) = ownership_path(host_session_id, instance_id, session_id) {
        let _ = fs::remove_file(path);
    }
    if let Ok(path) = process_locator_path(host_session_id, instance_id, session_id) {
        let _ = fs::remove_file(path);
    }
    remove_empty_ownership_dir(host_session_id);
}

pub fn remove_ownership_entry(entry: &OwnershipEntry) {
    let _ = fs::remove_file(&entry.path);
    if let Ok(path) = process_locator_path(
        &entry.record.host_session_id,
        &entry.record.instance_id,
        &entry.record.session_id,
    ) {
        let _ = fs::remove_file(path);
    }
    remove_empty_ownership_dir(&entry.record.host_session_id);
}

fn remove_empty_ownership_dir(host_session_id: &str) {
    let Ok(dir) = ownership_dir_path(host_session_id) else {
        return;
    };
    let is_empty = fs::read_dir(&dir)
        .map(|mut entries| entries.next().is_none())
        .unwrap_or(false);
    if is_empty {
        let _ = fs::remove_dir(dir);
    }
}

pub fn validate_id(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    {
        bail!("invalid identifier");
    }
    Ok(())
}

fn ensure_private_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path).with_context(|| format!("create directory {}", path.display()))?;
    set_private_dir(path)
}

fn write_private_json_new(path: &Path, value: &impl Serialize) -> Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    serde_json::to_writer(&mut file, value)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
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
