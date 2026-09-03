use std::{
    collections::HashMap,
    io::{Read, Write},
    path::PathBuf,
    sync::{Arc, Mutex, RwLock},
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
#[cfg(not(windows))]
use portable_pty::ChildKiller;
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream, tcp::OwnedReadHalf},
    sync::{Semaphore, broadcast},
    task::AbortHandle,
};
use uuid::Uuid;

use crate::{
    MAX_SESSIONS, OUTPUT_CAPACITY,
    buffer::{BufferRead, OutputBuffer},
    runtime::{self, BackgroundTaskTicket, ControlRecord, ProcessLocator},
};

const MAX_HANDSHAKE_BYTES: usize = 16 * 1024;
const MAX_CONTROL_CONNECTIONS: usize = 64;

#[derive(Debug, Clone, Copy, Serialize, schemars::JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    AwaitingBackgroundTask,
    Starting,
    Running,
    Finished,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, schemars::JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FinishReason {
    NaturalExit,
    StartFailed,
    ExplicitClose,
    BackgroundTaskDisconnected,
    HostSessionEnded,
    TicketExpired,
    ServerShutdown,
    Terminated,
    Killed,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema, PartialEq, Eq)]
pub struct Termination {
    pub reason: FinishReason,
    pub exit_code: Option<u32>,
    pub message: Option<String>,
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
    pub termination: Option<Termination>,
    pub created_at_ms: u64,
    pub last_activity_ms: u64,
    pub retained_start: u64,
    pub retained_end: u64,
    pub tail_text: String,
    pub tail_base64: String,
    pub tail_text_lossy: bool,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct BackgroundTaskLaunch {
    pub tool: String,
    pub command: String,
    pub run_in_background: bool,
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

struct Lifecycle {
    state: SessionState,
    rows: u16,
    cols: u16,
    last_activity_ms: u64,
    termination: Option<Termination>,
    requested_finish: Option<FinishReason>,
    owner: Option<String>,
    resources: Option<PtyResources>,
    expiry: Option<AbortHandle>,
}

struct PtyResources {
    writer: Box<dyn Write + Send>,
    master: Box<dyn MasterPty + Send>,
    #[cfg(not(windows))]
    killer: Box<dyn ChildKiller + Send + Sync>,
    #[cfg(windows)]
    job: PlatformJob,
    #[cfg(not(windows))]
    _job: PlatformJob,
    #[cfg(unix)]
    process_group: Option<i32>,
    #[cfg(unix)]
    process_id: Option<i32>,
}

impl PtyResources {
    #[cfg(unix)]
    fn process_locator(&self) -> Result<ProcessLocator> {
        Ok(ProcessLocator::Unix {
            process_id: self
                .process_id
                .ok_or_else(|| anyhow!("PTY child has no process id"))?,
            process_group: self.process_group,
        })
    }

    #[cfg(windows)]
    fn process_locator(&self) -> Result<ProcessLocator> {
        Ok(ProcessLocator::WindowsJob {
            name: self.job.name.clone(),
        })
    }
}

struct PtySetup {
    master: Box<dyn MasterPty + Send>,
    reader: Box<dyn Read + Send>,
    writer: Box<dyn Write + Send>,
    #[cfg(not(windows))]
    killer: Box<dyn ChildKiller + Send + Sync>,
    child: Box<dyn Child + Send + Sync>,
    #[cfg(windows)]
    job: PlatformJob,
    #[cfg(not(windows))]
    _job: PlatformJob,
    #[cfg(unix)]
    process_group: Option<i32>,
    #[cfg(unix)]
    process_id: Option<i32>,
}

impl PtySetup {
    #[cfg(unix)]
    fn process_locator(&self) -> Result<ProcessLocator> {
        Ok(ProcessLocator::Unix {
            process_id: self
                .process_id
                .ok_or_else(|| anyhow!("PTY child has no process id"))?,
            process_group: self.process_group,
        })
    }

    #[cfg(windows)]
    fn process_locator(&self) -> Result<ProcessLocator> {
        Ok(ProcessLocator::WindowsJob {
            name: self.job.name.clone(),
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum SessionEvent {
    Attached,
    Running { program: String },
    Output { preview: String },
    Finished { termination: Termination },
}

impl SessionEvent {
    fn is_finished(&self) -> bool {
        matches!(self, Self::Finished { .. })
    }
}

struct Session {
    id: String,
    spec: StartSpec,
    created_at_ms: u64,
    lifecycle: Mutex<Lifecycle>,
    output: Mutex<OutputBuffer>,
    events: broadcast::Sender<SessionEvent>,
}

impl Session {
    fn new(id: String, spec: StartSpec) -> Self {
        let (events, _) = broadcast::channel(256);
        let now = runtime::now_ms();
        Self {
            id,
            created_at_ms: now,
            lifecycle: Mutex::new(Lifecycle {
                state: SessionState::AwaitingBackgroundTask,
                rows: spec.rows,
                cols: spec.cols,
                last_activity_ms: now,
                termination: None,
                requested_finish: None,
                owner: None,
                resources: None,
                expiry: None,
            }),
            spec,
            output: Mutex::new(OutputBuffer::new(OUTPUT_CAPACITY)),
            events,
        }
    }

    fn event(&self, event: SessionEvent) {
        let _ = self.events.send(event);
    }

    fn record_output(&self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        self.output.lock().expect("output poisoned").append(bytes);
        self.lifecycle
            .lock()
            .expect("session lifecycle poisoned")
            .last_activity_ms = runtime::now_ms();
        let preview = sanitize_summary(bytes, 512);
        if !preview.is_empty() {
            self.event(SessionEvent::Output { preview });
        }
    }

    fn write_terminal_response(&self, bytes: &[u8]) -> Result<()> {
        if bytes.is_empty() {
            return Ok(());
        }
        let mut lifecycle = self.lifecycle.lock().expect("session lifecycle poisoned");
        if lifecycle.state != SessionState::Running {
            return Ok(());
        }
        let Some(resources) = lifecycle.resources.as_mut() else {
            return Ok(());
        };
        resources.writer.write_all(bytes)?;
        resources.writer.flush()?;
        lifecycle.last_activity_ms = runtime::now_ms();
        Ok(())
    }

    fn snapshot(&self) -> SessionSnapshot {
        let lifecycle = self.lifecycle.lock().expect("session lifecycle poisoned");
        let output = self.output.lock().expect("output buffer poisoned");
        let (retained_start, retained_end) = output.range();
        let tail = output.tail(4096);
        let tail_text_lossy = std::str::from_utf8(&tail).is_err();
        SessionSnapshot {
            session_id: self.id.clone(),
            program: self.spec.program.clone(),
            args: self.spec.args.clone(),
            cwd: self.spec.cwd.to_string_lossy().into_owned(),
            rows: lifecycle.rows,
            cols: lifecycle.cols,
            state: lifecycle.state,
            termination: lifecycle.termination.clone(),
            created_at_ms: self.created_at_ms,
            last_activity_ms: lifecycle.last_activity_ms,
            retained_start,
            retained_end,
            tail_text: String::from_utf8_lossy(&tail).into_owned(),
            tail_base64: BASE64.encode(tail),
            tail_text_lossy,
        }
    }
}

pub struct Manager {
    instance_id: String,
    port: u16,
    sessions: RwLock<HashMap<String, Arc<Session>>>,
    background_task_tokens: Mutex<HashMap<String, String>>,
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
            background_task_tokens: Mutex::new(HashMap::new()),
            control_tokens: Mutex::new(HashMap::new()),
        });
        let weak = Arc::downgrade(&manager);
        let slots = Arc::new(Semaphore::new(MAX_CONTROL_CONNECTIONS));
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let Ok(permit) = slots.clone().try_acquire_owned() else {
                    drop(stream);
                    continue;
                };
                let Some(manager) = weak.upgrade() else {
                    break;
                };
                tokio::spawn(async move {
                    let _permit = permit;
                    if let Err(error) = manager.handle_connection(stream).await {
                        tracing::debug!(%error, "control connection ended");
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
    ) -> Result<(String, BackgroundTaskLaunch)> {
        if !spec.cwd.is_dir() {
            bail!("working directory does not exist: {}", spec.cwd.display());
        }
        let executable = std::env::current_exe().context("resolve pty-bridge executable")?;
        let executable = executable
            .to_str()
            .context("pty-bridge executable path is not valid UTF-8")?;
        let session_id = format!("pty_{}", Uuid::new_v4().simple());
        let session = Arc::new(Session::new(session_id.clone(), spec));
        {
            let mut sessions = self.sessions.write().expect("sessions poisoned");
            while sessions.len() >= MAX_SESSIONS {
                let Some(id) = sessions
                    .iter()
                    .filter(|(_, session)| {
                        session
                            .lifecycle
                            .lock()
                            .expect("session lifecycle poisoned")
                            .state
                            == SessionState::Finished
                    })
                    .min_by_key(|(_, session)| session.created_at_ms)
                    .map(|(id, _)| id.clone())
                else {
                    bail!("session limit reached ({MAX_SESSIONS})");
                };
                sessions.remove(&id);
            }
            sessions.insert(session_id.clone(), session.clone());
        }

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
        self.background_task_tokens
            .lock()
            .expect("background task tokens poisoned")
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
        let expiry = tokio::spawn(async move {
            tokio::time::sleep(ticket_ttl).await;
            if let Some(manager) = weak.upgrade() {
                manager.expire_pending(&expiry_id);
            }
        });
        session
            .lifecycle
            .lock()
            .expect("session lifecycle poisoned")
            .expiry = Some(expiry.abort_handle());

        Ok((
            session_id.clone(),
            background_task_launch(executable, &self.instance_id, &session_id),
        ))
    }

    async fn handle_connection(self: Arc<Self>, stream: TcpStream) -> Result<()> {
        let (read_half, mut write_half) = stream.into_split();
        let mut reader = BufReader::new(read_half);
        let value = read_handshake(&mut reader).await?;
        match value.get("action").and_then(|value| value.as_str()) {
            Some("attach_background_task") => {
                let request: BackgroundTaskRequest = serde_json::from_value(value)?;
                self.handle_background_task(request, reader, &mut write_half)
                    .await
            }
            Some("bind_owner") => {
                let request: BindOwnerRequest = serde_json::from_value(value)?;
                self.validate_control(&request.instance_id, &request.session_id, &request.token)?;
                self.bind_owner(&request.session_id, &request.host_session_id)?;
                write_half.write_all(b"{\"ok\":true}\n").await?;
                Ok(())
            }
            Some("finish_owned") => {
                let request: CleanupRequest = serde_json::from_value(value)?;
                self.validate_control(&request.instance_id, &request.session_id, &request.token)?;
                self.finish(
                    &request.session_id,
                    Termination {
                        reason: FinishReason::HostSessionEnded,
                        exit_code: None,
                        message: None,
                    },
                    true,
                )?;
                write_half.write_all(b"{\"ok\":true}\n").await?;
                Ok(())
            }
            _ => bail!("unsupported action"),
        }
    }

    async fn handle_background_task(
        self: &Arc<Self>,
        request: BackgroundTaskRequest,
        mut reader: BufReader<OwnedReadHalf>,
        write_half: &mut tokio::net::tcp::OwnedWriteHalf,
    ) -> Result<()> {
        if request.instance_id != self.instance_id {
            bail!("instance mismatch");
        }
        let expected = self
            .background_task_tokens
            .lock()
            .expect("background task tokens poisoned")
            .get(&request.session_id)
            .cloned()
            .ok_or_else(|| anyhow!("background task ticket not found or already consumed"))?;
        if expected != request.token {
            bail!("background task ticket rejected");
        }
        let session = self.get(&request.session_id)?;
        {
            let mut lifecycle = session
                .lifecycle
                .lock()
                .expect("session lifecycle poisoned");
            if lifecycle.state != SessionState::AwaitingBackgroundTask {
                bail!("session is not waiting for its background task");
            }
            lifecycle.state = SessionState::Starting;
            lifecycle.last_activity_ms = runtime::now_ms();
            if let Some(expiry) = lifecycle.expiry.take() {
                expiry.abort();
            }
        }
        self.background_task_tokens
            .lock()
            .expect("background task tokens poisoned")
            .remove(&request.session_id);
        runtime::remove_background_task_ticket(&self.instance_id, &request.session_id);

        let mut events = session.events.subscribe();
        write_event(write_half, &SessionEvent::Attached).await?;
        if let Err(error) = self.start_session(&request.session_id).await {
            let message = error.to_string();
            let termination = Termination {
                reason: FinishReason::StartFailed,
                exit_code: None,
                message: Some(message),
            };
            let _ = self.finish(&request.session_id, termination.clone(), true);
            let _ = write_event(write_half, &SessionEvent::Finished { termination }).await;
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
                            let finished = event.is_finished();
                            if let Err(error) = write_event(write_half, &event).await {
                                break Err(error);
                            }
                            if finished {
                                break Ok(());
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(broadcast::error::RecvError::Closed) => {
                            break Err(anyhow!("session event channel closed before termination"));
                        }
                    }
                }
            }
        };

        if self.state(&request.session_id)? != SessionState::Finished {
            let _ = self.finish(
                &request.session_id,
                Termination {
                    reason: FinishReason::BackgroundTaskDisconnected,
                    exit_code: None,
                    message: result.as_ref().err().map(ToString::to_string),
                },
                true,
            );
        }
        result
    }

    async fn start_session(self: &Arc<Self>, session_id: &str) -> Result<()> {
        let session = self.get(session_id)?;
        let session_for_setup = session.clone();
        let setup_instance_id = self.instance_id.clone();
        let setup_session_id = session_id.to_owned();
        let setup = tokio::task::spawn_blocking(move || -> Result<PtySetup> {
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
            let job = assign_job(child.as_ref(), &setup_instance_id, &setup_session_id)?;
            drop(pair.slave);
            let reader = pair.master.try_clone_reader()?;
            let writer = pair.master.take_writer()?;
            #[cfg(not(windows))]
            let killer = child.clone_killer();
            #[cfg(unix)]
            let process_group = pair.master.process_group_leader();
            #[cfg(unix)]
            let process_id = child.process_id().and_then(|pid| i32::try_from(pid).ok());
            Ok(PtySetup {
                master: pair.master,
                reader,
                writer,
                #[cfg(not(windows))]
                killer,
                child,
                #[cfg(windows)]
                job,
                #[cfg(not(windows))]
                _job: job,
                #[cfg(unix)]
                process_group,
                #[cfg(unix)]
                process_id,
            })
        })
        .await??;

        let mut setup = setup;
        {
            let mut lifecycle = session
                .lifecycle
                .lock()
                .expect("session lifecycle poisoned");
            if lifecycle.state != SessionState::Starting {
                drop(lifecycle);
                stop_setup(&mut setup);
                let _ = setup.child.wait();
                return Ok(());
            }
            if let Some(owner) = lifecycle.owner.as_deref()
                && let Err(error) = setup.process_locator().and_then(|locator| {
                    runtime::write_process_locator(owner, &self.instance_id, session_id, &locator)
                })
            {
                drop(lifecycle);
                stop_setup(&mut setup);
                let _ = setup.child.wait();
                return Err(error).context("register owned PTY process");
            }
            lifecycle.resources = Some(PtyResources {
                writer: setup.writer,
                master: setup.master,
                #[cfg(not(windows))]
                killer: setup.killer,
                #[cfg(windows)]
                job: setup.job,
                #[cfg(not(windows))]
                _job: setup._job,
                #[cfg(unix)]
                process_group: setup.process_group,
                #[cfg(unix)]
                process_id: setup.process_id,
            });
            lifecycle.state = SessionState::Running;
            lifecycle.last_activity_ms = runtime::now_ms();
        }
        session.event(SessionEvent::Running {
            program: session.spec.program.clone(),
        });

        let read_session = session.clone();
        let mut reader = setup.reader;
        let reader_thread = std::thread::Builder::new()
            .name("pty-reader".into())
            .spawn(move || -> Option<String> {
                let mut buf = [0u8; 8192];
                let mut protocol = TerminalProtocol::default();
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) => {
                            read_session.record_output(&protocol.finish());
                            return None;
                        }
                        Ok(n) => {
                            let (visible, response) = protocol.process(&buf[..n]);
                            if let Err(error) = read_session.write_terminal_response(&response) {
                                return Some(format!("terminal protocol response failed: {error}"));
                            }
                            read_session.record_output(&visible);
                        }
                        Err(error) => return Some(error.to_string()),
                    }
                }
            })?;

        let manager = Arc::downgrade(self);
        let wait_id = session_id.to_string();
        let mut child = setup.child;
        std::thread::Builder::new()
            .name("pty-child-wait".into())
            .spawn(move || {
                let status = child.wait();
                if let Some(manager) = manager.upgrade() {
                    // ConPTY does not necessarily close the cloned reader while its master
                    // handle remains alive. Release the terminal handles after the child has
                    // exited, then let the reader drain everything already buffered before
                    // publishing the terminal event.
                    manager.release_resources_after_child_exit(&wait_id);
                    let read_error = reader_thread
                        .join()
                        .ok()
                        .flatten()
                        .map(|error| format!("terminal read failed: {error}"));
                    manager.child_finished(&wait_id, status, read_error);
                } else {
                    let _ = reader_thread.join();
                }
            })?;
        Ok(())
    }

    fn release_resources_after_child_exit(&self, session_id: &str) {
        let Ok(session) = self.get(session_id) else {
            return;
        };
        let resources = session
            .lifecycle
            .lock()
            .expect("session lifecycle poisoned")
            .resources
            .take();
        drop(resources);
    }

    fn child_finished(
        &self,
        session_id: &str,
        status: std::io::Result<portable_pty::ExitStatus>,
        read_error: Option<String>,
    ) {
        let Ok(session) = self.get(session_id) else {
            return;
        };
        let (reason, exit_code, message) = match status {
            Ok(status) => {
                let requested = session
                    .lifecycle
                    .lock()
                    .expect("session lifecycle poisoned")
                    .requested_finish;
                (
                    requested.unwrap_or(FinishReason::NaturalExit),
                    Some(status.exit_code()),
                    read_error,
                )
            }
            Err(error) => (
                FinishReason::StartFailed,
                None,
                Some(match read_error {
                    Some(read_error) => format!("child wait failed: {error}; {read_error}"),
                    None => format!("child wait failed: {error}"),
                }),
            ),
        };
        if self.state(session_id).ok() == Some(SessionState::Finished) {
            let mut lifecycle = session
                .lifecycle
                .lock()
                .expect("session lifecycle poisoned");
            if let Some(termination) = lifecycle.termination.as_mut()
                && termination.exit_code.is_none()
            {
                termination.exit_code = exit_code;
            }
            return;
        }
        let _ = self.finish(
            session_id,
            Termination {
                reason,
                exit_code,
                message,
            },
            false,
        );
    }

    pub async fn wait_until_running(&self, session_id: &str, timeout: Duration) -> Result<()> {
        let session = self.get(session_id)?;
        let mut events = session.events.subscribe();
        tokio::time::timeout(timeout, async {
            loop {
                let snapshot = session.snapshot();
                match snapshot.state {
                    SessionState::Running => return Ok(()),
                    SessionState::Finished => {
                        bail!(
                            "PTY finished before becoming writable: {:?}",
                            snapshot.termination
                        )
                    }
                    SessionState::AwaitingBackgroundTask | SessionState::Starting => {}
                }
                events
                    .recv()
                    .await
                    .context("session event channel closed")?;
            }
        })
        .await
        .context("timed out waiting for PTY to become writable")?
    }

    pub fn write(&self, session_id: &str, text: &str) -> Result<()> {
        let session = self.get(session_id)?;
        let mut lifecycle = session
            .lifecycle
            .lock()
            .expect("session lifecycle poisoned");
        if lifecycle.state != SessionState::Running {
            bail!("PTY is not running (state: {:?})", lifecycle.state);
        }
        let resources = lifecycle
            .resources
            .as_mut()
            .ok_or_else(|| anyhow!("PTY resources are unavailable"))?;
        resources.writer.write_all(text.as_bytes())?;
        resources.writer.flush()?;
        lifecycle.last_activity_ms = runtime::now_ms();
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

    pub async fn wait_for_output(
        &self,
        session_id: &str,
        cursor: u64,
        timeout: Duration,
    ) -> Result<()> {
        if timeout.is_zero() {
            return Ok(());
        }
        let session = self.get(session_id)?;
        let mut events = session.events.subscribe();
        if session.output.lock().expect("output poisoned").range().1 > cursor
            || self.state(session_id)? == SessionState::Finished
        {
            return Ok(());
        }
        let _ = tokio::time::timeout(timeout, events.recv()).await;
        Ok(())
    }

    pub fn resize(&self, session_id: &str, rows: u16, cols: u16) -> Result<()> {
        if rows == 0 || cols == 0 {
            bail!("rows and cols must be greater than zero");
        }
        let session = self.get(session_id)?;
        let mut lifecycle = session
            .lifecycle
            .lock()
            .expect("session lifecycle poisoned");
        if lifecycle.state != SessionState::Running {
            bail!("PTY is not running (state: {:?})", lifecycle.state);
        }
        lifecycle
            .resources
            .as_ref()
            .ok_or_else(|| anyhow!("PTY resources are unavailable"))?
            .master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })?;
        lifecycle.rows = rows;
        lifecycle.cols = cols;
        lifecycle.last_activity_ms = runtime::now_ms();
        Ok(())
    }

    pub fn interrupt(&self, session_id: &str) -> Result<()> {
        self.write(session_id, "\u{3}")
    }

    pub fn terminate(&self, session_id: &str, force: bool) -> Result<()> {
        let session = self.get(session_id)?;
        let mut lifecycle = session
            .lifecycle
            .lock()
            .expect("session lifecycle poisoned");
        if lifecycle.state != SessionState::Running {
            bail!("PTY is not running (state: {:?})", lifecycle.state);
        }
        lifecycle.requested_finish = Some(if force {
            FinishReason::Killed
        } else {
            FinishReason::Terminated
        });
        let resources = lifecycle
            .resources
            .as_mut()
            .ok_or_else(|| anyhow!("PTY resources are unavailable"))?;
        signal_resources(resources, force)
    }

    pub fn close(&self, session_id: &str) -> Result<()> {
        self.finish(
            session_id,
            Termination {
                reason: FinishReason::ExplicitClose,
                exit_code: None,
                message: None,
            },
            true,
        )
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
            .lifecycle
            .lock()
            .expect("session lifecycle poisoned")
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

    fn validate_control(&self, instance_id: &str, session_id: &str, token: &str) -> Result<()> {
        if instance_id != self.instance_id {
            bail!("instance mismatch");
        }
        let expected = self
            .control_tokens
            .lock()
            .expect("control tokens poisoned")
            .get(session_id)
            .cloned()
            .ok_or_else(|| anyhow!("control credential not found"))?;
        if expected != token {
            bail!("control credential rejected");
        }
        Ok(())
    }

    fn bind_owner(&self, session_id: &str, host_session_id: &str) -> Result<()> {
        runtime::validate_id(host_session_id)?;
        let session = self.get(session_id)?;
        let mut lifecycle = session
            .lifecycle
            .lock()
            .expect("session lifecycle poisoned");
        if lifecycle.state == SessionState::Finished {
            bail!("session already finished");
        }
        if let Some(resources) = lifecycle.resources.as_ref() {
            runtime::write_process_locator(
                host_session_id,
                &self.instance_id,
                session_id,
                &resources.process_locator()?,
            )
            .context("register owned PTY process")?;
        }
        lifecycle.owner = Some(host_session_id.to_string());
        Ok(())
    }

    fn expire_pending(&self, session_id: &str) {
        let Ok(session) = self.get(session_id) else {
            return;
        };
        let should_expire = session
            .lifecycle
            .lock()
            .expect("session lifecycle poisoned")
            .state
            == SessionState::AwaitingBackgroundTask;
        if should_expire {
            let _ = self.finish_if_state(
                session_id,
                Termination {
                    reason: FinishReason::TicketExpired,
                    exit_code: None,
                    message: Some("background task ticket expired".into()),
                },
                false,
                Some(SessionState::AwaitingBackgroundTask),
            );
        }
    }

    fn finish(&self, session_id: &str, termination: Termination, stop: bool) -> Result<()> {
        self.finish_if_state(session_id, termination, stop, None)
            .map(|_| ())
    }

    fn finish_if_state(
        &self,
        session_id: &str,
        termination: Termination,
        stop: bool,
        required_state: Option<SessionState>,
    ) -> Result<bool> {
        let session = self.get(session_id)?;
        let (resources, owner, expiry) = {
            let mut lifecycle = session
                .lifecycle
                .lock()
                .expect("session lifecycle poisoned");
            if lifecycle.state == SessionState::Finished {
                return Ok(false);
            }
            if required_state.is_some_and(|state| lifecycle.state != state) {
                return Ok(false);
            }
            lifecycle.state = SessionState::Finished;
            lifecycle.last_activity_ms = runtime::now_ms();
            lifecycle.termination = Some(termination.clone());
            (
                lifecycle.resources.take(),
                lifecycle.owner.take(),
                lifecycle.expiry.take(),
            )
        };
        if let Some(expiry) = expiry {
            expiry.abort();
        }
        if stop && let Some(resources) = resources {
            force_stop(resources);
        }
        self.remove_credentials(session_id);
        if let Some(owner) = owner {
            runtime::remove_ownership_record(&owner, &self.instance_id, session_id);
        }
        session.event(SessionEvent::Finished { termination });
        Ok(true)
    }

    fn remove_credentials(&self, session_id: &str) {
        self.background_task_tokens
            .lock()
            .expect("background task tokens poisoned")
            .remove(session_id);
        self.control_tokens
            .lock()
            .expect("control tokens poisoned")
            .remove(session_id);
        runtime::remove_session_credentials(&self.instance_id, session_id);
    }

    fn rollback_create(&self, session_id: &str) {
        self.sessions
            .write()
            .expect("sessions poisoned")
            .remove(session_id);
        self.remove_credentials(session_id);
    }
}

#[cfg(windows)]
#[derive(Debug)]
struct KillJob {
    handle: isize,
    name: String,
}

#[cfg(windows)]
type PlatformJob = KillJob;

#[cfg(not(windows))]
struct NoopJob;

#[cfg(not(windows))]
type PlatformJob = NoopJob;

#[cfg(windows)]
fn assign_job(child: &dyn Child, instance_id: &str, session_id: &str) -> Result<PlatformJob> {
    KillJob::assign(child, instance_id, session_id)
}

#[cfg(not(windows))]
fn assign_job(_child: &dyn Child, _instance_id: &str, _session_id: &str) -> Result<PlatformJob> {
    Ok(NoopJob)
}

#[cfg(windows)]
impl KillJob {
    fn assign(child: &dyn Child, instance_id: &str, session_id: &str) -> Result<Self> {
        use std::{ffi::c_void, mem::size_of, ptr::null};
        use windows_sys::Win32::{
            Foundation::{CloseHandle, HANDLE},
            System::JobObjects::{
                AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
                JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
                SetInformationJobObject,
            },
        };

        let name = format!("Local\\pty-bridge-{instance_id}-{session_id}");
        let wide_name: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        let job = unsafe { CreateJobObjectW(null(), wide_name.as_ptr()) };
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
        Ok(Self { handle: raw, name })
    }

    fn terminate(&self) -> Result<()> {
        use windows_sys::Win32::{Foundation::HANDLE, System::JobObjects::TerminateJobObject};
        if unsafe { TerminateJobObject(self.handle as HANDLE, 1) } == 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(())
    }
}

#[cfg(windows)]
impl Drop for KillJob {
    fn drop(&mut self) {
        use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
        unsafe { CloseHandle(self.handle as HANDLE) };
    }
}

fn signal_resources(resources: &mut PtyResources, force: bool) -> Result<()> {
    #[cfg(unix)]
    if let Some(group) = resources.process_group {
        let signal = if force { libc::SIGKILL } else { libc::SIGTERM };
        if unsafe { libc::kill(-group, signal) } == 0 {
            return Ok(());
        }
    }
    #[cfg(unix)]
    if let Some(process_id) = resources.process_id {
        let signal = if force { libc::SIGKILL } else { libc::SIGTERM };
        if unsafe { libc::kill(process_id, signal) } == 0 {
            return Ok(());
        }
    }
    #[cfg(windows)]
    {
        let _ = force;
        resources.job.terminate()
    }
    #[cfg(not(windows))]
    resources.killer.kill().map_err(Into::into)
}

fn force_stop(mut resources: PtyResources) {
    let _ = signal_resources(&mut resources, true);
    drop(resources);
}

fn stop_setup(setup: &mut PtySetup) {
    #[cfg(unix)]
    if let Some(group) = setup.process_group
        && unsafe { libc::kill(-group, libc::SIGKILL) } == 0
    {
        return;
    }
    #[cfg(unix)]
    if let Some(process_id) = setup.process_id
        && unsafe { libc::kill(process_id, libc::SIGKILL) } == 0
    {
        return;
    }
    #[cfg(windows)]
    {
        let _ = setup.job.terminate();
    }
    #[cfg(not(windows))]
    {
        let _ = setup.killer.kill();
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
            let _ = self.finish(
                &id,
                Termination {
                    reason: FinishReason::ServerShutdown,
                    exit_code: None,
                    message: None,
                },
                true,
            );
        }
        runtime::remove_instance(&self.instance_id);
    }
}

#[derive(Debug, Deserialize)]
struct BackgroundTaskRequest {
    #[serde(rename = "action")]
    _action: String,
    instance_id: String,
    session_id: String,
    token: String,
}

#[derive(Debug, Deserialize)]
struct BindOwnerRequest {
    #[serde(rename = "action")]
    _action: String,
    instance_id: String,
    session_id: String,
    host_session_id: String,
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

async fn read_handshake(reader: &mut BufReader<OwnedReadHalf>) -> Result<serde_json::Value> {
    let mut data = Vec::with_capacity(512);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let byte = reader.read_u8().await?;
            if byte == b'\n' {
                break;
            }
            if data.len() >= MAX_HANDSHAKE_BYTES {
                bail!("connection handshake exceeds {MAX_HANDSHAKE_BYTES} bytes");
            }
            data.push(byte);
        }
        Result::<()>::Ok(())
    })
    .await
    .context("connection handshake timeout")??;
    serde_json::from_slice(&data).context("parse connection handshake")
}

async fn write_event(
    writer: &mut tokio::net::tcp::OwnedWriteHalf,
    event: &SessionEvent,
) -> Result<()> {
    writer.write_all(&serde_json::to_vec(event)?).await?;
    writer.write_all(b"\n").await?;
    Ok(())
}

fn background_task_launch(
    executable: &str,
    instance_id: &str,
    session_id: &str,
) -> BackgroundTaskLaunch {
    #[cfg(windows)]
    let (tool, command) = (
        "PowerShell",
        format!(
            "& {} background-task --instance {} --session {}",
            powershell_quote(executable),
            powershell_quote(instance_id),
            powershell_quote(session_id)
        ),
    );
    #[cfg(not(windows))]
    let (tool, command) = (
        "Bash",
        format!(
            "{} background-task --instance {} --session {}",
            shell_quote(executable),
            shell_quote(instance_id),
            shell_quote(session_id)
        ),
    );
    BackgroundTaskLaunch {
        tool: tool.into(),
        command,
        run_in_background: true,
    }
}

#[cfg(not(windows))]
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

#[cfg(windows)]
fn powershell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn random_token() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    BASE64.encode(bytes)
}

fn sanitize_summary(bytes: &[u8], max_chars: usize) -> String {
    String::from_utf8_lossy(bytes)
        .chars()
        .filter(|character| {
            *character == '\n'
                || *character == '\r'
                || *character == '\t'
                || !character.is_control()
        })
        .take(max_chars)
        .collect()
}

#[derive(Default)]
struct TerminalProtocol {
    pending: Vec<u8>,
}

impl TerminalProtocol {
    fn process(&mut self, bytes: &[u8]) -> (Vec<u8>, Vec<u8>) {
        const QUERIES: [(&[u8], &[u8]); 3] = [
            (b"\x1b[5n", b"\x1b[0n"),
            (b"\x1b[6n", b"\x1b[1;1R"),
            (b"\x1b[?6n", b"\x1b[?1;1R"),
        ];

        self.pending.extend_from_slice(bytes);
        let mut visible = Vec::with_capacity(self.pending.len());
        let mut response = Vec::new();
        let mut consumed = 0;
        while consumed < self.pending.len() {
            let rest = &self.pending[consumed..];
            if let Some((query, answer)) = QUERIES.iter().find(|(query, _)| rest.starts_with(query))
            {
                consumed += query.len();
                response.extend_from_slice(answer);
                continue;
            }
            if QUERIES.iter().any(|(query, _)| query.starts_with(rest)) {
                break;
            }
            visible.push(self.pending[consumed]);
            consumed += 1;
        }
        self.pending.drain(..consumed);
        (visible, response)
    }

    fn finish(self) -> Vec<u8> {
        self.pending
    }
}

pub fn default_spec(program: String, args: Vec<String>, cwd: PathBuf) -> StartSpec {
    StartSpec {
        program,
        args,
        cwd,
        env: HashMap::new(),
        rows: crate::DEFAULT_ROWS,
        cols: crate::DEFAULT_COLS,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(not(windows))]
    #[test]
    fn shell_command_quotes_executable_paths() {
        let launch = background_task_launch("/tmp/a b'c/pty-bridge", "inst_x", "pty_y");
        assert_eq!(launch.tool, "Bash");
        assert!(launch.command.starts_with("'/tmp/a b'\"'\"'c/pty-bridge'"));
    }

    #[test]
    fn sanitizes_control_characters() {
        assert_eq!(sanitize_summary(b"a\x00b\n", 8), "ab\n");
    }

    #[test]
    fn answers_terminal_status_queries_across_read_boundaries() {
        let mut protocol = TerminalProtocol::default();
        let (visible, response) = protocol.process(b"before\x1b[");
        assert_eq!(visible, b"before");
        assert!(response.is_empty());

        let (visible, response) = protocol.process(b"6nafter\x1b[5n");
        assert_eq!(visible, b"after");
        assert_eq!(response, b"\x1b[1;1R\x1b[0n");
        assert!(protocol.finish().is_empty());
    }
}
