use std::{collections::HashMap, path::PathBuf, sync::Arc, time::Duration};

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
    manager::{Manager, SessionSnapshot, SessionState, StartSpec},
};

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
    pub background_task_command: String,
    pub next_action: String,
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
pub struct ReadResponse {
    pub session_id: String,
    pub output: String,
    pub next_cursor: u64,
    pub dropped_bytes: u64,
    pub utf8_lossy: bool,
    pub state: SessionState,
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
}

#[tool_router]
impl PtyServer {
    #[tool(
        name = "start",
        description = "Prepare a real interactive PTY session. The process waits until you immediately run the returned background_task_command with the Bash tool using run_in_background=true. Use the command exactly as returned; it contains no secret."
    )]
    async fn start(
        &self,
        Parameters(req): Parameters<StartRequest>,
    ) -> Result<Json<StartResponse>, String> {
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
        let (session_id, background_task_command) = self
            .manager
            .create(spec, Duration::from_secs(ttl))
            .map_err(|e| e.to_string())?;
        Ok(Json(StartResponse {
            instance_id: self.manager.instance_id().to_string(),
            session_id,
            control_port: self.manager.port(),
            state: SessionState::AwaitingBackgroundTask,
            next_action: "run_background_task".into(),
            background_task_command,
        }))
    }

    #[tool(
        name = "read",
        description = "Read non-destructive incremental PTY output from a byte cursor."
    )]
    async fn read(
        &self,
        Parameters(req): Parameters<ReadRequest>,
    ) -> Result<Json<ReadResponse>, String> {
        let wait = req.yield_time_ms.unwrap_or(0).min(30_000);
        if wait > 0 {
            tokio::time::sleep(Duration::from_millis(wait)).await;
        }
        let data = self
            .manager
            .read(
                &req.session_id,
                req.cursor,
                req.max_output_bytes.unwrap_or(64 * 1024).min(1024 * 1024),
            )
            .map_err(|e| e.to_string())?;
        let utf8_lossy = std::str::from_utf8(&data.bytes).is_err();
        Ok(Json(ReadResponse {
            session_id: req.session_id.clone(),
            output: String::from_utf8_lossy(&data.bytes).into_owned(),
            next_cursor: data.next_cursor,
            dropped_bytes: data.dropped_bytes,
            utf8_lossy,
            state: self
                .manager
                .state(&req.session_id)
                .map_err(|e| e.to_string())?,
        }))
    }

    #[tool(
        name = "write",
        description = "Write UTF-8 text or terminal control characters to a running PTY session."
    )]
    async fn write(
        &self,
        Parameters(req): Parameters<WriteRequest>,
    ) -> Result<Json<ReadResponse>, String> {
        let cursor = self
            .manager
            .snapshots(Some(&req.session_id))
            .map_err(|e| e.to_string())?[0]
            .retained_end;
        self.manager
            .write(&req.session_id, &req.text)
            .map_err(|e| e.to_string())?;
        tokio::time::sleep(Duration::from_millis(
            req.yield_time_ms.unwrap_or(250).min(30_000),
        ))
        .await;
        let data = self
            .manager
            .read(&req.session_id, cursor, 64 * 1024)
            .map_err(|e| e.to_string())?;
        let utf8_lossy = std::str::from_utf8(&data.bytes).is_err();
        Ok(Json(ReadResponse {
            session_id: req.session_id.clone(),
            output: String::from_utf8_lossy(&data.bytes).into_owned(),
            next_cursor: data.next_cursor,
            dropped_bytes: data.dropped_bytes,
            utf8_lossy,
            state: self
                .manager
                .state(&req.session_id)
                .map_err(|e| e.to_string())?,
        }))
    }

    #[tool(name = "resize", description = "Resize a running PTY session.")]
    fn resize(
        &self,
        Parameters(req): Parameters<ResizeRequest>,
    ) -> Result<Json<ActionResponse>, String> {
        self.manager
            .resize(&req.session_id, req.rows, req.cols)
            .map_err(|e| e.to_string())?;
        Ok(Json(ActionResponse {
            session_id: req.session_id,
            state: SessionState::Running,
        }))
    }

    #[tool(
        name = "signal",
        description = "Send interrupt, terminate, or kill semantics to a PTY session."
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
        .map_err(|e| e.to_string())?;
        Ok(Json(ActionResponse {
            session_id: req.session_id.clone(),
            state: self
                .manager
                .state(&req.session_id)
                .unwrap_or(SessionState::Closed),
        }))
    }

    #[tool(
        name = "status",
        description = "Inspect one PTY session or list all sessions, including retained output ranges and recent output."
    )]
    fn status(
        &self,
        Parameters(req): Parameters<StatusRequest>,
    ) -> Result<Json<StatusResponse>, String> {
        Ok(Json(StatusResponse {
            sessions: self
                .manager
                .snapshots(req.session_id.as_deref())
                .map_err(|e| e.to_string())?,
        }))
    }

    #[tool(name = "close", description = "Terminate and release a PTY session.")]
    fn close(
        &self,
        Parameters(req): Parameters<CloseRequest>,
    ) -> Result<Json<ActionResponse>, String> {
        self.manager
            .close(&req.session_id)
            .map_err(|e| e.to_string())?;
        Ok(Json(ActionResponse {
            session_id: req.session_id,
            state: SessionState::Closed,
        }))
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for PtyServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(
            "Use these tools for commands that need a real interactive terminal. After start returns next_action=run_background_task, immediately invoke Bash with background_task_command exactly as returned and run_in_background=true. Never run it in the foreground. The Background Task provides the live status display and owns the PTY lifetime. Use read/write/resize/signal for subsequent interaction.",
        ).with_server_info(
            Implementation::new("pty-bridge", env!("CARGO_PKG_VERSION"))
                .with_title("PTY Bridge")
                .with_description("Cross-platform interactive terminal sessions"),
        )
    }
}
