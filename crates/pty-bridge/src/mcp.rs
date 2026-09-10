use crate::manager::Manager;
use pty_core::StartSpec;
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
use serde_json::{Value, json};
use std::{collections::HashMap, path::PathBuf, sync::Arc, time::Duration};

/// MCP requires an object at the root of every output schema.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ToolResponse {
    #[serde(flatten)]
    fields: serde_json::Map<String, Value>,
}
fn response(value: Value) -> Json<ToolResponse> {
    Json(ToolResponse {
        fields: value
            .as_object()
            .expect("tool responses are objects")
            .clone(),
    })
}

#[derive(Clone)]
pub struct PtyServer {
    pub manager: Arc<Manager>,
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
#[serde(deny_unknown_fields)]
pub struct StartRequest {
    pub program: String,
    #[serde(default)]
    pub args: Vec<String>,
    pub cwd: Option<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    pub rows: Option<u16>,
    pub cols: Option<u16>,
    /// Injected by the plugin hook from the current Claude session; do not invent a value.
    pub host_session_id: Option<String>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadRequest {
    pub session_id: String,
    #[serde(default)]
    pub cursor: u64,
    pub max_output_bytes: Option<usize>,
    pub yield_time_ms: Option<u64>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WriteRequest {
    pub session_id: String,
    pub text: String,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ResizeRequest {
    pub session_id: String,
    pub rows: u16,
    pub cols: u16,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
pub struct StatusRequest {
    pub session_id: Option<String>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CloseRequest {
    pub session_id: String,
}

#[tool_router]
impl PtyServer {
    #[tool(
        name = "start",
        description = "Start a real PTY and child immediately, after the session's plugin monitor is ready. Success confirms process creation, not command completion or application readiness. Immediately run background_task.command using background_task.tool with run_in_background=true to register completion/failure notifications. If that call fails or is denied, close the PTY."
    )]
    async fn start(
        &self,
        Parameters(req): Parameters<StartRequest>,
    ) -> Result<Json<ToolResponse>, String> {
        if std::env::var_os("CLAUDE_CODE_DISABLE_BACKGROUND_TASKS").is_some_and(|v| v != "0") {
            return Err("Background Tasks are disabled; PTY Bridge requires the native monitor and a bgshell per PTY".into());
        }
        let host = req
            .host_session_id
            .ok_or("missing host_session_id; the PTY Bridge PreToolUse hook must be enabled")?;
        let spec = StartSpec {
            program: req.program,
            args: req.args,
            cwd: req
                .cwd
                .map(PathBuf::from)
                .or_else(|| std::env::var_os("PTY_BRIDGE_PROJECT_DIR").map(PathBuf::from))
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_default()),
            env: req.env,
            rows: req.rows.unwrap_or(24),
            cols: req.cols.unwrap_or(80),
        };
        let entry = self
            .manager
            .start(&host, spec)
            .await
            .map_err(|e| format!("{e:#}"))?;
        let launch = match wait_launch(self.manager.instance_id(), &entry.id, self.manager.port()) {
            Ok(launch) => launch,
            Err(error) => {
                let _ = self.manager.close(&entry.id);
                return Err(error.to_string());
            }
        };
        Ok(response(
            json!({"instance_id":self.manager.instance_id(),"control_port":self.manager.port(),"session_id":entry.id,
            "state":entry.session.snapshot().state,"termination":entry.snapshot()["termination"],
            "background_task":launch,"next_action":"run_background_task"}),
        ))
    }

    #[tool(
        name = "read",
        description = "Read retained terminal bytes by independent cursor; this is the only tool returning terminal output. start_cursor..next_cursor is the actual returned range; dropped_bytes were lost, not read. base64 preserves all returned bytes. Wait up to yield_time_ms (maximum 30000) without polling. An empty read does not reset silence detection."
    )]
    async fn read(
        &self,
        Parameters(req): Parameters<ReadRequest>,
    ) -> Result<Json<ToolResponse>, String> {
        let max = req.max_output_bytes.unwrap_or(64 * 1024).min(1024 * 1024);
        if max == 0 {
            return Err("max_output_bytes must be greater than zero".into());
        }
        self.manager
            .read(
                &req.session_id,
                req.cursor,
                max,
                Duration::from_millis(req.yield_time_ms.unwrap_or(0).min(30000)),
            )
            .await
            .map(response)
            .map_err(|e| e.to_string())
    }
    #[tool(
        name = "write",
        description = "Write UTF-8 input/control bytes through the single writer. Returns confirmed bytes_written and interaction_id, with no terminal output. Success does not prove the input was interpreted as a command. Use read to inspect the result. On partial/uncertain failure do not retry automatically."
    )]
    async fn write(
        &self,
        Parameters(req): Parameters<WriteRequest>,
    ) -> Result<Json<ToolResponse>, String> {
        if req.text.len() > 64 * 1024 {
            return Err("text exceeds 65536 UTF-8 bytes".into());
        }
        let entry = self
            .manager
            .get(&req.session_id)
            .map_err(|e| e.to_string())?;
        let receipt = entry
            .session
            .writer()
            .write(req.text.as_bytes())
            .await
            .map_err(|e| serde_json::to_string(&e).unwrap())?;
        Ok(response(
            json!({"session_id":req.session_id,"bytes_written":receipt.bytes_written,"interaction_id":receipt.interaction_id,
            "state":entry.session.snapshot().state,"termination":entry.snapshot()["termination"]}),
        ))
    }
    #[tool(
        name = "resize",
        description = "Resize the running terminal. Resizing does not restart the input interaction or silence timer."
    )]
    fn resize(
        &self,
        Parameters(req): Parameters<ResizeRequest>,
    ) -> Result<Json<ToolResponse>, String> {
        let entry = self
            .manager
            .get(&req.session_id)
            .map_err(|e| e.to_string())?;
        entry
            .session
            .resize(req.rows, req.cols)
            .map_err(|e| e.to_string())?;
        Ok(response(entry.snapshot()))
    }
    #[tool(
        name = "signal",
        description = "Interrupt sends Ctrl-C through the writer. Terminate/kill signal the process tree independently of a blocked writer."
    )]
    async fn signal(
        &self,
        Parameters(req): Parameters<SignalRequest>,
    ) -> Result<Json<ToolResponse>, String> {
        match req.signal {
            Signal::Interrupt => {
                let entry = self
                    .manager
                    .get(&req.session_id)
                    .map_err(|e| e.to_string())?;
                entry
                    .session
                    .writer()
                    .write(b"\x03")
                    .await
                    .map_err(|e| e.to_string())?;
            }
            Signal::Terminate => self
                .manager
                .signal(&req.session_id, false)
                .map_err(|e| e.to_string())?,
            Signal::Kill => self
                .manager
                .signal(&req.session_id, true)
                .map_err(|e| e.to_string())?,
        }
        Ok(response(
            self.manager
                .get(&req.session_id)
                .map_err(|e| e.to_string())?
                .snapshot(),
        ))
    }
    #[tool(
        name = "status",
        description = "Inspect session lifecycle, termination, dimensions, activity times and retained byte range. Contains no terminal text and does not acknowledge output as read."
    )]
    fn status(
        &self,
        Parameters(req): Parameters<StatusRequest>,
    ) -> Result<Json<ToolResponse>, String> {
        Ok(response(
            json!({"sessions":self.manager.snapshots(req.session_id.as_deref()).map_err(|e|e.to_string())?}),
        ))
    }
    #[tool(
        name = "close",
        description = "Abandon and force-stop a PTY. Idempotent; finished output remains readable without requiring close."
    )]
    fn close(
        &self,
        Parameters(req): Parameters<CloseRequest>,
    ) -> Result<Json<ToolResponse>, String> {
        self.manager
            .close(&req.session_id)
            .map_err(|e| e.to_string())?;
        Ok(response(
            self.manager
                .get(&req.session_id)
                .map_err(|e| e.to_string())?
                .snapshot(),
        ))
    }
}

pub fn wait_launch(instance: &str, session: &str, port: u16) -> anyhow::Result<Value> {
    let exe = std::env::current_exe()?.to_string_lossy().into_owned();
    #[cfg(not(windows))]
    {
        Ok(
            json!({"tool":"Bash","command":format!("{} wait --instance {} --session {} --port {port}",shell_quote(&exe),shell_quote(instance),shell_quote(session)),"run_in_background":true}),
        )
    }
    #[cfg(windows)]
    {
        Ok(
            json!({"tool":"PowerShell","command":format!("& {} wait --instance {} --session {} --port {port}; exit $LASTEXITCODE",shell_quote(&exe),shell_quote(instance),shell_quote(session)),"run_in_background":true}),
        )
    }
}
#[cfg(not(windows))]
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}
#[cfg(windows)]
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

#[tool_handler(router=self.tool_router)]
impl ServerHandler for PtyServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(
            "Use PTY Bridge for real interactive terminals. The plugin monitor must be continuously online. start actually creates the target, then returns an exact bgshell wait command: run background_task.tool with background_task.command and run_in_background=true immediately, including for a target that already finished. This wait command does not start the target. If registering the background task fails or is denied, call close. write only confirms bytes written; read is the only source of terminal output. Do not assume a write executed a command or made an application ready. A monitor notice means only suspected silence; call read and decide what to do. Finished sessions retain bounded output and need no close. Never retry partial or uncertain writes automatically. No Skill invocation is required.")
            .with_server_info(Implementation::new("pty-bridge",env!("CARGO_PKG_VERSION")).with_title("PTY Bridge"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn every_mcp_schema_has_an_object_root() {
        let tools = PtyServer::tool_router().list_all();
        assert_eq!(tools.len(), 7);
        for tool in tools {
            assert_eq!(
                tool.input_schema.get("type"),
                Some(&json!("object")),
                "{} input",
                tool.name
            );
            assert_eq!(
                tool.output_schema.unwrap().get("type"),
                Some(&json!("object")),
                "{} output",
                tool.name
            );
        }
    }
    #[test]
    fn removed_input_fields_are_rejected() {
        assert!(
            serde_json::from_value::<WriteRequest>(
                json!({"session_id":"pty_x","text":"x","yield_time_ms":2})
            )
            .is_err()
        );
        assert!(
            serde_json::from_value::<StartRequest>(
                json!({"program":"sh","background_task_ticket_ttl_seconds":30})
            )
            .is_err()
        );
    }
    #[test]
    fn wait_command_quotes_paths_and_has_no_start_phase() {
        let launch = wait_launch("inst_x", "pty_x", 123).unwrap();
        let command = launch["command"].as_str().unwrap();
        assert!(command.contains(" wait "));
        assert!(command.contains("--port 123"));
        assert!(shell_quote("a'b c").starts_with('\''));
    }
}
