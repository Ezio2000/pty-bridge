use std::{
    collections::HashMap,
    io::{Read, Write},
    path::PathBuf,
    sync::{Arc, Mutex, RwLock},
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use portable_pty::{ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
    sync::broadcast,
};
use uuid::Uuid;

use crate::{
    DEFAULT_COLS, DEFAULT_ROWS, MAX_SESSIONS, OUTPUT_CAPACITY,
    buffer::{BufferRead, OutputBuffer},
    runtime::{self, BackgroundTaskTicket, ControlRecord},
};

#[derive(Debug, Clone, Copy, Serialize, schemars::JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    AwaitingBackgroundTask,
    Starting,
    Running,
    Exited,
    Failed,
    Closed,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct SessionSnapshot {
    pub session_id: String,
    pub program: String,
    pub args: Vec<String>,
    pub cwd: String,
    pub rows: u16,
    pub cols: u16,
    pub state: SessionState,
    pub created_at_ms: u64,
    pub last_activity_ms: u64,
    pub retained_start: u64,
    pub retained_end: u64,
    pub exit_code: Option<u32>,
    pub tail: String,
}

#[derive(Debug, Clone)]
pub struct StartSpec {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub env: HashMap<String, String>,
    pub rows: u16,
    pub cols: u16,
}

struct SessionMeta {
    state: SessionState,
    rows: u16,
    cols: u16,
    last_activity_ms: u64,
    exit_code: Option<u32>,
}

struct Session {
    id: String,
    spec: StartSpec,
    created_at_ms: u64,
    meta: Mutex<SessionMeta>,
    output: Mutex<OutputBuffer>,
    writer: Mutex<Option<Box<dyn Write + Send>>>,
    master: Mutex<Option<Box<dyn MasterPty + Send>>>,
    killer: Mutex<Option<Box<dyn ChildKiller + Send + Sync>>>,
    job: Mutex<Option<PlatformJob>>,
    events: broadcast::Sender<String>,
}

impl Session {
    fn new(id: String, spec: StartSpec, state: SessionState) -> Self {
        let (events, _) = broadcast::channel(256);
        Self {
            id,
            created_at_ms: runtime::now_ms(),
            meta: Mutex::new(SessionMeta {
                state,
                rows: spec.rows,
                cols: spec.cols,
                last_activity_ms: runtime::now_ms(),
                exit_code: None,
            }),
            spec,
            output: Mutex::new(OutputBuffer::new(OUTPUT_CAPACITY)),
            writer: Mutex::new(None),
            master: Mutex::new(None),
            killer: Mutex::new(None),
            job: Mutex::new(None),
            events,
        }
    }

    fn event(&self, event: impl Into<String>) {
        let _ = self.events.send(event.into());
    }

    fn set_state(&self, state: SessionState) {
        let mut meta = self.meta.lock().expect("session meta poisoned");
        meta.state = state;
        meta.last_activity_ms = runtime::now_ms();
    }

    fn snapshot(&self) -> SessionSnapshot {
        let meta = self.meta.lock().expect("session meta poisoned");
        let output = self.output.lock().expect("output buffer poisoned");
        let (retained_start, retained_end) = output.range();
        SessionSnapshot {
            session_id: self.id.clone(),
            program: self.spec.program.clone(),
            args: self.spec.args.clone(),
            cwd: self.spec.cwd.to_string_lossy().into_owned(),
            rows: meta.rows,
            cols: meta.cols,
            state: meta.state,
            created_at_ms: self.created_at_ms,
            last_activity_ms: meta.last_activity_ms,
            retained_start,
            retained_end,
            exit_code: meta.exit_code,
            tail: String::from_utf8_lossy(&output.tail(4096)).into_owned(),
        }
    }
}

pub struct Manager {
    instance_id: String,
    port: u16,
    sessions: RwLock<HashMap<String, Arc<Session>>>,
    background_task_tickets: Mutex<HashMap<String, String>>,
    control_tokens: Mutex<HashMap<String, String>>,
}

impl Manager {
    pub async fn new() -> Result<Arc<Self>> {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
        let port = listener.local_addr()?.port();
        let manager = Arc::new(Self {
            instance_id: format!("inst_{}", Uuid::new_v4().simple()),
            port,
            sessions: RwLock::new(HashMap::new()),
            background_task_tickets: Mutex::new(HashMap::new()),
            control_tokens: Mutex::new(HashMap::new()),
        });
        let weak = Arc::downgrade(&manager);
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let Some(manager) = weak.upgrade() else {
                    break;
                };
                tokio::spawn(async move {
                    if let Err(error) = manager.handle_connection(stream).await {
                        tracing::debug!(%error, "background task connection ended");
                    }
                });
            }
        });
        Ok(manager)
    }

    pub fn instance_id(&self) -> &str {
        &self.instance_id
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn create(
        self: &Arc<Self>,
        spec: StartSpec,
        ticket_ttl: Duration,
    ) -> Result<(String, String)> {
        self.prune_exited();
        if self.sessions.read().expect("sessions poisoned").len() >= MAX_SESSIONS {
            bail!("session limit reached ({MAX_SESSIONS})");
        }
        if !spec.cwd.is_dir() {
            bail!("working directory does not exist: {}", spec.cwd.display());
        }
        let executable = std::env::current_exe().context("resolve pty-bridge executable")?;
        let session_id = format!("pty_{}", Uuid::new_v4().simple());
        let session = Arc::new(Session::new(
            session_id.clone(),
            spec,
            SessionState::AwaitingBackgroundTask,
        ));
        self.sessions
            .write()
            .expect("sessions poisoned")
            .insert(session_id.clone(), session.clone());

        let control_token = random_token();
        self.control_tokens
            .lock()
            .expect("control tokens poisoned")
            .insert(session_id.clone(), control_token.clone());
        if let Err(error) = runtime::write_control(&ControlRecord {
            instance_id: self.instance_id.clone(),
            session_id: session_id.clone(),
            port: self.port,
            token: control_token,
        }) {
            self.rollback_create(&session_id);
            return Err(error);
        }

        let token = random_token();
        self.background_task_tickets
            .lock()
            .expect("background task tickets poisoned")
            .insert(session_id.clone(), token.clone());
        let ticket = BackgroundTaskTicket {
            instance_id: self.instance_id.clone(),
            session_id: session_id.clone(),
            port: self.port,
            token,
            expires_at_ms: runtime::now_ms() + ticket_ttl.as_millis() as u64,
        };
        if let Err(error) = runtime::write_background_task_ticket(&ticket) {
            self.rollback_create(&session_id);
            return Err(error);
        }
        let weak = Arc::downgrade(self);
        let expiry_id = session_id.clone();
        tokio::spawn(async move {
            tokio::time::sleep(ticket_ttl).await;
            if let Some(manager) = weak.upgrade() {
                manager.expire_pending(&expiry_id);
            }
        });
        let background_task_command = format!(
            "\"{}\" background-task --instance {} --session {}",
            executable.display(),
            self.instance_id,
            session_id
        );
        Ok((session_id, background_task_command))
    }

    async fn handle_connection(self: Arc<Self>, stream: TcpStream) -> Result<()> {
        let (read_half, mut write_half) = stream.into_split();
        let mut reader = BufReader::new(read_half);
        let mut line = String::new();
        tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut line))
            .await
            .context("connection handshake timeout")??;
        let value: serde_json::Value = serde_json::from_str(line.trim())?;
        if value.get("action").and_then(|v| v.as_str()) == Some("cleanup") {
            let request: CleanupRequest = serde_json::from_value(value)?;
            if request.instance_id != self.instance_id {
                bail!("instance mismatch");
            }
            let expected = self
                .control_tokens
                .lock()
                .expect("control tokens poisoned")
                .get(&request.session_id)
                .cloned()
                .ok_or_else(|| anyhow!("control credential not found"))?;
            if expected != request.token {
                bail!("control credential rejected");
            }
            self.close(&request.session_id)?;
            write_half.write_all(b"{\"ok\":true}\n").await?;
            return Ok(());
        }
        let request: BackgroundTaskRequest = serde_json::from_value(value)?;
        if request.action != "attach_background_task" {
            bail!("unsupported action");
        }
        if request.instance_id != self.instance_id {
            bail!("instance mismatch");
        }
        let expected = self
            .background_task_tickets
            .lock()
            .expect("background task tickets poisoned")
            .remove(&request.session_id)
            .ok_or_else(|| anyhow!("background task ticket not found or already consumed"))?;
        if expected != request.token {
            bail!("background task ticket rejected");
        }
        runtime::remove_background_task_ticket(&self.instance_id, &request.session_id);
        let session = self.get(&request.session_id)?;
        {
            let mut meta = session.meta.lock().expect("session meta poisoned");
            if meta.state != SessionState::AwaitingBackgroundTask {
                bail!("session is not waiting for its background task");
            }
            meta.state = SessionState::Starting;
        }
        write_half
            .write_all(b"[starting] background task attached\n")
            .await?;
        let mut events = session.events.subscribe();
        if let Err(error) = self.start_session(&request.session_id).await {
            self.fail_session(&request.session_id, &error.to_string());
            write_half
                .write_all(format!("[failed] {error}\n").as_bytes())
                .await?;
            return Err(error);
        }
        let mut disconnect_probe = [0u8; 1];
        let result = loop {
            tokio::select! {
                read = reader.read(&mut disconnect_probe) => {
                    match read {
                        Ok(0) => break Ok(()),
                        Ok(_) => continue,
                        Err(error) => break Err(error.into()),
                    }
                }
                event = events.recv() => {
                    match event {
                        Ok(event) => {
                            write_half.write_all(event.as_bytes()).await?;
                            write_half.write_all(b"\n").await?;
                            if matches!(session.meta.lock().expect("session meta poisoned").state, SessionState::Exited | SessionState::Failed | SessionState::Closed) {
                                break Ok(());
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(broadcast::error::RecvError::Closed) => break Ok(()),
                    }
                }
            }
        };
        if !matches!(
            session.meta.lock().expect("session meta poisoned").state,
            SessionState::Exited | SessionState::Failed | SessionState::Closed
        ) {
            let _ = self.close(&request.session_id);
        }
        result
    }

    async fn start_session(self: &Arc<Self>, session_id: &str) -> Result<()> {
        let session = self.get(session_id)?;
        let session_for_setup = session.clone();
        let setup = tokio::task::spawn_blocking(move || -> Result<_> {
            let pty_system = native_pty_system();
            let pair = pty_system.openpty(PtySize {
                rows: session_for_setup.spec.rows,
                cols: session_for_setup.spec.cols,
                pixel_width: 0,
                pixel_height: 0,
            })?;
            let mut cmd = CommandBuilder::new(&session_for_setup.spec.program);
            cmd.args(&session_for_setup.spec.args);
            cmd.cwd(&session_for_setup.spec.cwd);
            for (key, value) in &session_for_setup.spec.env {
                cmd.env(key, value);
            }
            let child = pair.slave.spawn_command(cmd)?;
            let job = assign_job(child.as_ref())?;
            drop(pair.slave);
            let reader = pair.master.try_clone_reader()?;
            let writer = pair.master.take_writer()?;
            let killer = child.clone_killer();
            Ok((pair.master, reader, writer, killer, child, job))
        })
        .await??;

        let (master, mut reader, writer, killer, mut child, job) = setup;
        *session.master.lock().expect("master poisoned") = Some(master);
        *session.writer.lock().expect("writer poisoned") = Some(writer);
        *session.killer.lock().expect("killer poisoned") = Some(killer);
        *session.job.lock().expect("job poisoned") = Some(job);
        session.set_state(SessionState::Running);
        session.event(format!("[running] {}", session.spec.program));

        let read_session = session.clone();
        std::thread::Builder::new()
            .name("pty-reader".into())
            .spawn(move || {
                let mut buf = [0u8; 8192];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            {
                                let mut output =
                                    read_session.output.lock().expect("output poisoned");
                                output.append(&buf[..n]);
                            }
                            read_session
                                .meta
                                .lock()
                                .expect("meta poisoned")
                                .last_activity_ms = runtime::now_ms();
                            let summary = sanitize_summary(&buf[..n], 512);
                            if !summary.is_empty() {
                                read_session.event(format!("[output] {summary}"));
                            }
                        }
                        Err(error) => {
                            read_session.event(format!("[read-error] {error}"));
                            break;
                        }
                    }
                }
            })?;

        let wait_session = session.clone();
        std::thread::Builder::new()
            .name("pty-child-wait".into())
            .spawn(move || match child.wait() {
                Ok(status) => {
                    {
                        let mut meta = wait_session.meta.lock().expect("meta poisoned");
                        if meta.state != SessionState::Closed {
                            meta.state = SessionState::Exited;
                        }
                        meta.exit_code = Some(status.exit_code());
                        meta.last_activity_ms = runtime::now_ms();
                    }
                    wait_session.event(format!("[exited] code={}", status.exit_code()));
                    wait_session.writer.lock().expect("writer poisoned").take();
                    wait_session.master.lock().expect("master poisoned").take();
                    wait_session.killer.lock().expect("killer poisoned").take();
                    wait_session.job.lock().expect("job poisoned").take();
                }
                Err(error) => {
                    if wait_session.meta.lock().expect("meta poisoned").state
                        != SessionState::Closed
                    {
                        wait_session.set_state(SessionState::Failed);
                    }
                    wait_session.event(format!("[failed] {error}"));
                    wait_session.writer.lock().expect("writer poisoned").take();
                    wait_session.master.lock().expect("master poisoned").take();
                    wait_session.killer.lock().expect("killer poisoned").take();
                    wait_session.job.lock().expect("job poisoned").take();
                }
            })?;
        Ok(())
    }

    pub fn write(&self, session_id: &str, text: &str) -> Result<()> {
        let session = self.get(session_id)?;
        let mut writer = session.writer.lock().expect("writer poisoned");
        let writer = writer
            .as_mut()
            .ok_or_else(|| anyhow!("PTY is not running"))?;
        writer.write_all(text.as_bytes())?;
        writer.flush()?;
        Ok(())
    }

    pub fn read(&self, session_id: &str, cursor: u64, max_bytes: usize) -> Result<BufferRead> {
        let session = self.get(session_id)?;
        let result = session
            .output
            .lock()
            .expect("output poisoned")
            .read(cursor, max_bytes);
        Ok(result)
    }

    pub fn resize(&self, session_id: &str, rows: u16, cols: u16) -> Result<()> {
        if rows == 0 || cols == 0 {
            bail!("rows and cols must be greater than zero");
        }
        let session = self.get(session_id)?;
        let master = session.master.lock().expect("master poisoned");
        master
            .as_ref()
            .ok_or_else(|| anyhow!("PTY is not running"))?
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })?;
        let mut meta = session.meta.lock().expect("meta poisoned");
        meta.rows = rows;
        meta.cols = cols;
        Ok(())
    }

    pub fn interrupt(&self, session_id: &str) -> Result<()> {
        self.write(session_id, "\u{3}")
    }

    pub fn terminate(&self, session_id: &str, _force: bool) -> Result<()> {
        let session = self.get(session_id)?;
        #[cfg(unix)]
        {
            let master = session.master.lock().expect("master poisoned");
            if let Some(group) = master
                .as_ref()
                .and_then(|master| master.process_group_leader())
            {
                let signal = if _force { libc::SIGKILL } else { libc::SIGTERM };
                let result = unsafe { libc::kill(-group, signal) };
                if result == 0 {
                    return Ok(());
                }
            }
        }
        let mut killer = session.killer.lock().expect("killer poisoned");
        killer
            .as_mut()
            .ok_or_else(|| anyhow!("PTY is not running"))?
            .kill()?;
        Ok(())
    }

    pub fn close(&self, session_id: &str) -> Result<()> {
        let session = self.get(session_id)?;
        if let Some(killer) = session.killer.lock().expect("killer poisoned").as_mut() {
            let _ = killer.kill();
        }
        session.job.lock().expect("job poisoned").take();
        session.writer.lock().expect("writer poisoned").take();
        session.master.lock().expect("master poisoned").take();
        session.set_state(SessionState::Closed);
        session.event("[closed] session terminated");
        runtime::remove_background_task_ticket(&self.instance_id, session_id);
        runtime::remove_control(&self.instance_id, session_id);
        self.background_task_tickets
            .lock()
            .expect("background task tickets poisoned")
            .remove(session_id);
        self.control_tokens
            .lock()
            .expect("control tokens poisoned")
            .remove(session_id);
        Ok(())
    }

    pub fn snapshots(&self, session_id: Option<&str>) -> Result<Vec<SessionSnapshot>> {
        if let Some(id) = session_id {
            return Ok(vec![self.get(id)?.snapshot()]);
        }
        let sessions = self.sessions.read().expect("sessions poisoned");
        Ok(sessions
            .values()
            .map(|session| session.snapshot())
            .collect())
    }

    pub fn state(&self, session_id: &str) -> Result<SessionState> {
        Ok(self
            .get(session_id)?
            .meta
            .lock()
            .expect("meta poisoned")
            .state)
    }

    fn get(&self, session_id: &str) -> Result<Arc<Session>> {
        self.sessions
            .read()
            .expect("sessions poisoned")
            .get(session_id)
            .cloned()
            .ok_or_else(|| anyhow!("session not found: {session_id}"))
    }

    fn expire_pending(&self, session_id: &str) {
        let Ok(session) = self.get(session_id) else {
            return;
        };
        if session.meta.lock().expect("meta poisoned").state == SessionState::AwaitingBackgroundTask
        {
            session.set_state(SessionState::Failed);
            session.event("[failed] background task ticket expired");
            runtime::remove_background_task_ticket(&self.instance_id, session_id);
            self.background_task_tickets
                .lock()
                .expect("background task tickets poisoned")
                .remove(session_id);
            self.control_tokens
                .lock()
                .expect("control tokens poisoned")
                .remove(session_id);
            runtime::remove_control(&self.instance_id, session_id);
        }
    }

    fn rollback_create(&self, session_id: &str) {
        self.sessions
            .write()
            .expect("sessions poisoned")
            .remove(session_id);
        self.background_task_tickets
            .lock()
            .expect("background task tickets poisoned")
            .remove(session_id);
        self.control_tokens
            .lock()
            .expect("control tokens poisoned")
            .remove(session_id);
        runtime::remove_background_task_ticket(&self.instance_id, session_id);
        runtime::remove_control(&self.instance_id, session_id);
    }

    fn fail_session(&self, session_id: &str, message: &str) {
        if let Ok(session) = self.get(session_id) {
            session.set_state(SessionState::Failed);
            session.event(format!("[failed] {message}"));
        }
    }

    fn prune_exited(&self) {
        let mut sessions = self.sessions.write().expect("sessions poisoned");
        if sessions.len() < MAX_SESSIONS {
            return;
        }
        if let Some(id) = sessions
            .iter()
            .filter(|(_, session)| {
                matches!(
                    session.meta.lock().expect("meta poisoned").state,
                    SessionState::Exited | SessionState::Failed | SessionState::Closed
                )
            })
            .min_by_key(|(_, session)| session.created_at_ms)
            .map(|(id, _)| id.clone())
        {
            sessions.remove(&id);
        }
    }
}

#[cfg(windows)]
#[derive(Debug)]
struct KillJob(isize);

#[cfg(windows)]
type PlatformJob = KillJob;

#[cfg(not(windows))]
struct NoopJob;

#[cfg(not(windows))]
type PlatformJob = NoopJob;

#[cfg(windows)]
fn assign_job(child: &dyn portable_pty::Child) -> Result<PlatformJob> {
    KillJob::assign(child)
}

#[cfg(not(windows))]
fn assign_job(_child: &dyn portable_pty::Child) -> Result<PlatformJob> {
    Ok(NoopJob)
}

#[cfg(windows)]
impl KillJob {
    fn assign(child: &dyn portable_pty::Child) -> Result<Self> {
        use std::{ffi::c_void, mem::size_of, ptr::null};
        use windows_sys::Win32::{
            Foundation::{CloseHandle, HANDLE},
            System::JobObjects::{
                AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
                JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
                SetInformationJobObject,
            },
        };

        let job = unsafe { CreateJobObjectW(null(), null()) };
        if job.is_null() {
            return Err(std::io::Error::last_os_error().into());
        }
        let raw = job as isize;
        let result = (|| -> Result<()> {
            let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            if unsafe {
                SetInformationJobObject(
                    job,
                    JobObjectExtendedLimitInformation,
                    &limits as *const _ as *const c_void,
                    size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                )
            } == 0
            {
                return Err(std::io::Error::last_os_error().into());
            }
            let process = child
                .as_raw_handle()
                .ok_or_else(|| anyhow!("PTY child has no Windows process handle"))?
                as HANDLE;
            if unsafe { AssignProcessToJobObject(job, process) } == 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            Ok(())
        })();
        if let Err(error) = result {
            unsafe { CloseHandle(job) };
            return Err(error);
        }
        Ok(Self(raw))
    }
}

#[cfg(windows)]
impl Drop for KillJob {
    fn drop(&mut self) {
        use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
        unsafe { CloseHandle(self.0 as HANDLE) };
    }
}

impl Drop for Manager {
    fn drop(&mut self) {
        let ids: Vec<_> = self
            .sessions
            .read()
            .expect("sessions poisoned")
            .keys()
            .cloned()
            .collect();
        for id in ids {
            let _ = self.close(&id);
        }
        if let Ok(dir) = runtime::instance_dir(&self.instance_id) {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

#[derive(Debug, Deserialize)]
struct BackgroundTaskRequest {
    action: String,
    instance_id: String,
    session_id: String,
    token: String,
}

#[derive(Debug, Deserialize)]
struct CleanupRequest {
    #[serde(rename = "action")]
    _action: String,
    instance_id: String,
    session_id: String,
    token: String,
}

fn random_token() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn sanitize_summary(data: &[u8], max_chars: usize) -> String {
    let text = String::from_utf8_lossy(data);
    let mut result = String::new();
    let mut escape = false;
    for ch in text.chars() {
        if result.chars().count() >= max_chars {
            break;
        }
        if escape {
            if ch.is_ascii_alphabetic() || ch == '~' {
                escape = false;
            }
            continue;
        }
        if ch == '\u{1b}' {
            escape = true;
        } else if ch == '\n' || ch == '\r' {
            if !result.ends_with(' ') {
                result.push(' ');
            }
        } else if !ch.is_control() {
            result.push(ch);
        }
    }
    result.trim().to_string()
}

pub fn default_spec(program: String, args: Vec<String>, cwd: PathBuf) -> StartSpec {
    StartSpec {
        program,
        args,
        cwd,
        env: HashMap::new(),
        rows: DEFAULT_ROWS,
        cols: DEFAULT_COLS,
    }
}
