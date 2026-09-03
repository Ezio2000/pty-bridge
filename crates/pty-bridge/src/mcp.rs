use std::{collections::HashMap, path::PathBuf, sync::Arc, time::Duration};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use rmcp::{
    ServerHandler,
    handler::server::{
        router::tool::ToolRouter,
        wrapper::{Json, Parameters},
    },
    model::{Implementation, ServerCapabilities, ServerInfo},
    schemars, tool, tool_handler, tool_router,
};
use serde::{Deserialize, Serialize};

use crate::{
    DEFAULT_BACKGROUND_TASK_TICKET_TTL_SECONDS, DEFAULT_COLS, DEFAULT_ROWS,
    manager::{
        BackgroundTaskLaunch, Manager, SessionSnapshot, SessionState, StartSpec, Termination,
    },
};

const DEFAULT_WRITE_READY_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_INPUT_BYTES: usize = 64 * 1024;

#[derive(Clone)]
pub struct PtyServer {
    manager: Arc<Manager>,
    tool_router: ToolRouter<Self>,
}

impl PtyServer {
    pub async fn new() -> anyhow::Result<Self> {
        Ok(Self {
            manager: Manager::new().await?,
            tool_router: Self::tool_router(),
        })
    }
}

#[derive(Debug, Clone, Copy, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum NextAction {
    RunBackgroundTask,
    Interact,
    None,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct StartRequest {
    pub program: String,
    #[serde(default)]
    pub args: Vec<String>,
    pub cwd: Option<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    pub rows: Option<u16>,
    pub cols: Option<u16>,
    pub background_task_ticket_ttl_seconds: Option<u64>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct StartResponse {
    pub instance_id: String,
    pub session_id: String,
    pub control_port: u16,
    pub state: SessionState,
    pub background_task: BackgroundTaskLaunch,
    pub next_action: NextAction,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ReadRequest {
    pub session_id: String,
    #[serde(default)]
    pub cursor: u64,
    pub max_output_bytes: Option<usize>,
    pub yield_time_ms: Option<u64>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct OutputData {
    pub text: String,
    pub base64: String,
    pub text_lossy: bool,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct OutputResponse {
    pub session_id: String,
    pub output: OutputData,
    pub next_cursor: u64,
    pub dropped_bytes: u64,
    pub state: SessionState,
    pub termination: Option<Termination>,
    pub next_action: NextAction,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WriteRequest {
    pub session_id: String,
    pub text: String,
    pub yield_time_ms: Option<u64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ResizeRequest {
    pub session_id: String,
    pub rows: u16,
    pub cols: u16,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SignalRequest {
    pub session_id: String,
    pub signal: Signal,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Signal {
    Interrupt,
    Terminate,
    Kill,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct StatusRequest {
    pub session_id: Option<String>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct StatusResponse {
    pub sessions: Vec<SessionSnapshot>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CloseRequest {
    pub session_id: String,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ActionResponse {
    pub session_id: String,
    pub state: SessionState,
    pub termination: Option<Termination>,
    pub next_action: NextAction,
}

#[tool_router]
impl PtyServer {
    #[tool(
        name = "start",
        description = "Create a real interactive PTY. Immediately run background_task.command with background_task.tool and run_in_background=true exactly as returned. The target process cannot start before that Background Task attaches; the returned command contains no secret."
    )]
    async fn start(
        &self,
        Parameters(req): Parameters<StartRequest>,
    ) -> Result<Json<StartResponse>, String> {
        if std::env::var_os("CLAUDE_CODE_DISABLE_BACKGROUND_TASKS")
            .is_some_and(|value| value != "0")
        {
            return Err(
                "Background Tasks are disabled; PTY Bridge cannot establish lifecycle ownership"
                    .into(),
            );
        }
        if req.program.is_empty() {
            return Err("program must not be empty".into());
        }
        let ttl = req
            .background_task_ticket_ttl_seconds
            .unwrap_or(DEFAULT_BACKGROUND_TASK_TICKET_TTL_SECONDS);
        if !(30..=86_400).contains(&ttl) {
            return Err("background_task_ticket_ttl_seconds must be between 30 and 86400".into());
        }
        let cwd = req
            .cwd
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("PTY_BRIDGE_PROJECT_DIR").map(PathBuf::from))
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
        let rows = req.rows.unwrap_or(DEFAULT_ROWS);
        let cols = req.cols.unwrap_or(DEFAULT_COLS);
        if rows == 0 || cols == 0 {
            return Err("rows and cols must be greater than zero".into());
        }
        let spec = StartSpec {
            program: req.program,
            args: req.args,
            cwd,
            env: req.env,
            rows,
            cols,
        };
        let (session_id, background_task) = self
            .manager
            .create(spec, Duration::from_secs(ttl))
            .map_err(|error| error.to_string())?;
        Ok(Json(StartResponse {
            instance_id: self.manager.instance_id().to_string(),
            session_id,
            control_port: self.manager.port(),
            state: SessionState::AwaitingBackgroundTask,
            background_task,
            next_action: NextAction::RunBackgroundTask,
        }))
    }

    #[tool(
        name = "read",
        description = "Read retained PTY bytes from an independent cursor. output.text is convenient text; output.base64 is the lossless source when text_lossy=true. The call waits for new output up to yield_time_ms without polling."
    )]
    async fn read(
        &self,
        Parameters(req): Parameters<ReadRequest>,
    ) -> Result<Json<OutputResponse>, String> {
        let wait = Duration::from_millis(req.yield_time_ms.unwrap_or(0).min(30_000));
        self.manager
            .wait_for_output(&req.session_id, req.cursor, wait)
            .await
            .map_err(|error| error.to_string())?;
        self.output_response(
            &req.session_id,
            req.cursor,
            req.max_output_bytes.unwrap_or(64 * 1024).min(1024 * 1024),
        )
    }

    #[tool(
        name = "write",
        description = "Write UTF-8 text or control characters to a PTY. If attachment is still starting, this waits briefly until the PTY is writable, then waits for prompt output without polling."
    )]
    async fn write(
        &self,
        Parameters(req): Parameters<WriteRequest>,
    ) -> Result<Json<OutputResponse>, String> {
        if req.text.len() > MAX_INPUT_BYTES {
            return Err(format!("text exceeds {MAX_INPUT_BYTES} UTF-8 bytes"));
        }
        self.manager
            .wait_until_running(&req.session_id, DEFAULT_WRITE_READY_TIMEOUT)
            .await
            .map_err(|error| error.to_string())?;
        let cursor = self
            .manager
            .snapshots(Some(&req.session_id))
            .map_err(|error| error.to_string())?[0]
            .retained_end;
        let manager = Arc::clone(&self.manager);
        let write_session_id = req.session_id.clone();
        let text = req.text;
        tokio::task::spawn_blocking(move || manager.write(&write_session_id, &text))
            .await
            .map_err(|error| error.to_string())?
            .map_err(|error| error.to_string())?;
        self.manager
            .wait_for_output(
                &req.session_id,
                cursor,
                Duration::from_millis(req.yield_time_ms.unwrap_or(250).min(30_000)),
            )
            .await
            .map_err(|error| error.to_string())?;
        self.output_response(&req.session_id, cursor, 64 * 1024)
    }

    #[tool(name = "resize", description = "Resize a running PTY terminal.")]
    fn resize(
        &self,
        Parameters(req): Parameters<ResizeRequest>,
    ) -> Result<Json<ActionResponse>, String> {
        self.manager
            .resize(&req.session_id, req.rows, req.cols)
            .map_err(|error| error.to_string())?;
        self.action_response(&req.session_id)
    }

    #[tool(
        name = "signal",
        description = "Send interrupt, terminate, or kill semantics to a running PTY process tree."
    )]
    fn signal(
        &self,
        Parameters(req): Parameters<SignalRequest>,
    ) -> Result<Json<ActionResponse>, String> {
        match req.signal {
            Signal::Interrupt => self.manager.interrupt(&req.session_id),
            Signal::Terminate => self.manager.terminate(&req.session_id, false),
            Signal::Kill => self.manager.terminate(&req.session_id, true),
        }
        .map_err(|error| error.to_string())?;
        self.action_response(&req.session_id)
    }

    #[tool(
        name = "status",
        description = "Inspect PTY lifecycle, explicit termination reason, terminal dimensions, retained byte range, and recent output."
    )]
    fn status(
        &self,
        Parameters(req): Parameters<StatusRequest>,
    ) -> Result<Json<StatusResponse>, String> {
        Ok(Json(StatusResponse {
            sessions: self
                .manager
                .snapshots(req.session_id.as_deref())
                .map_err(|error| error.to_string())?,
        }))
    }

    #[tool(
        name = "close",
        description = "Finish and force-stop a session only when abandoning a target that is still waiting, starting, or running. Finished sessions are already finalized automatically, so do not close them."
    )]
    fn close(
        &self,
        Parameters(req): Parameters<CloseRequest>,
    ) -> Result<Json<ActionResponse>, String> {
        self.manager
            .close(&req.session_id)
            .map_err(|error| error.to_string())?;
        self.action_response(&req.session_id)
    }
}

impl PtyServer {
    fn output_response(
        &self,
        session_id: &str,
        cursor: u64,
        max_bytes: usize,
    ) -> Result<Json<OutputResponse>, String> {
        let data = self
            .manager
            .read(session_id, cursor, max_bytes)
            .map_err(|error| error.to_string())?;
        let snapshot = self
            .manager
            .snapshots(Some(session_id))
            .map_err(|error| error.to_string())?
            .into_iter()
            .next()
            .ok_or_else(|| "session disappeared".to_string())?;
        Ok(Json(OutputResponse {
            session_id: session_id.to_string(),
            output: OutputData {
                text: String::from_utf8_lossy(&data.bytes).into_owned(),
                base64: BASE64.encode(&data.bytes),
                text_lossy: std::str::from_utf8(&data.bytes).is_err(),
            },
            next_cursor: data.next_cursor,
            dropped_bytes: data.dropped_bytes,
            state: snapshot.state,
            termination: snapshot.termination,
            next_action: next_action(snapshot.state),
        }))
    }

    fn action_response(&self, session_id: &str) -> Result<Json<ActionResponse>, String> {
        let snapshot = self
            .manager
            .snapshots(Some(session_id))
            .map_err(|error| error.to_string())?
            .into_iter()
            .next()
            .ok_or_else(|| "session disappeared".to_string())?;
        Ok(Json(ActionResponse {
            session_id: session_id.to_string(),
            state: snapshot.state,
            termination: snapshot.termination,
            next_action: next_action(snapshot.state),
        }))
    }
}

fn next_action(state: SessionState) -> NextAction {
    match state {
        SessionState::AwaitingBackgroundTask => NextAction::RunBackgroundTask,
        SessionState::Starting | SessionState::Running => NextAction::Interact,
        SessionState::Finished => NextAction::None,
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for PtyServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions(
                "Use PTY Bridge for commands that require a real terminal, including interactive prompts, REPLs, debuggers, installers, and TUIs. Call start, then immediately invoke background_task.tool with background_task.command exactly as returned and run_in_background=true; never run that command in the foreground or modify it. Wait only for the host to confirm the Background Task started, then use read/write/resize/signal. Follow next_action on every response. state=finished is fully finalized: output remains readable, credentials are removed, and close is neither required nor appropriate. Call close only to abandon a session that is still waiting, starting, or running. No Skill invocation is required.",
            )
            .with_server_info(
                Implementation::new("pty-bridge", env!("CARGO_PKG_VERSION"))
                    .with_title("PTY Bridge")
                    .with_description("Cross-platform interactive terminal sessions"),
            )
    }
}
