use crate::{manager::Manager, waiting::WaitOptions};
use pty_core::{StartSpec, keys};
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
use tokio_util::sync::CancellationToken;

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
    /// Executable to start with args. Give exactly one of program or command.
    pub program: Option<String>,
    /// Shell command line run by the login shell ($SHELL -lc; cmd.exe /C on Windows).
    pub command: Option<String>,
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
    /// auto (default): screen while the program uses the alternate screen, otherwise text.
    /// text: plain text of new output, with line-editor redraws resolved.
    /// screen: current emulated screen with cursor and reverse-video highlights.
    /// raw: terminal bytes including escape sequences.
    #[serde(default)]
    pub mode: ReadMode,
    pub max_output_bytes: Option<usize>,
    /// Maximum wait in milliseconds (capped at 30000). Defaults to 0, or 10000 with idle_ms or until.
    pub yield_time_ms: Option<u64>,
    /// Return once new output has been quiet this long. With until it defaults to 5000; 0 disables it.
    pub idle_ms: Option<u64>,
    /// Regex (multi-line) returning as soon as it matches new output: rendered text after cursor,
    /// or a screen row that was not already matching. A pattern may include or omit spaces after a prompt.
    pub until: Option<String>,
}
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ReadMode {
    #[default]
    Auto,
    Text,
    Screen,
    Raw,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WriteRequest {
    pub session_id: String,
    /// UTF-8 input. Give exactly one of text or keys.
    pub text: Option<String>,
    /// Named keys written in order as one input: Enter, Tab, Esc, Backspace, Space, Up, Down,
    /// Left, Right, Home, End, PageUp, PageDown, Insert, Delete, F1-F12 or a single character,
    /// with optional C-, M- and S- prefixes (C-c, M-x, S-Tab, C-Left).
    pub keys: Option<Vec<String>>,
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
        description = "Start a real PTY and child immediately, after the session's plugin monitor is ready. Give program with args, or a shell command line. The terminal defaults to 40 rows by 120 columns; programs lay out their output to this size, so pass larger rows/cols for wide tables or long lists. Success confirms process creation, not command completion or application readiness. Immediately run background_task.command using background_task.tool with run_in_background=true to register completion/failure notifications. If that call fails or is denied, close the PTY."
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
        let (program, args) = match (req.program, req.command) {
            (Some(program), None) => (program, req.args),
            (None, Some(command)) if req.args.is_empty() => shell_command(command),
            (None, Some(_)) => return Err("args cannot be combined with command".into()),
            _ => return Err("provide exactly one of program or command".into()),
        };
        let spec = StartSpec {
            program,
            args,
            cwd: req
                .cwd
                .map(PathBuf::from)
                .or_else(|| std::env::var_os("PTY_BRIDGE_PROJECT_DIR").map(PathBuf::from))
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_default()),
            env: req.env,
            rows: req.rows.unwrap_or(DEFAULT_ROWS),
            cols: req.cols.unwrap_or(DEFAULT_COLS),
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
        Ok(response(start_result(&entry.id, &entry.snapshot(), launch)))
    }

    #[tool(
        name = "read",
        description = "Read terminal output; this is the only tool returning it. Pass the previous next_cursor as cursor. text mode returns new output as plain text in text; screen mode returns the whole current screen as screen.lines with cursor [row, col] (use it for menus, full-screen programs and prompts redrawn in place); auto picks screen for full-screen programs, else text. start_cursor (only when it differs from cursor) and dropped_bytes (only when nonzero) report output lost before it was read. Without idle_ms or until, waits up to yield_time_ms (maximum 30000) for the first output beyond cursor. idle_ms returns once new output goes quiet; until returns as soon as a regex matches new output, falling back to 5 s of quiet. wait.reason reports matched, idle, output, exited, limit, timeout or cancelled; a match does not prove the program is ready. An empty read does not reset silence detection."
    )]
    async fn read(
        &self,
        Parameters(req): Parameters<ReadRequest>,
        cancel: CancellationToken,
    ) -> Result<Json<ToolResponse>, String> {
        let max = req.max_output_bytes.unwrap_or(64 * 1024).min(1024 * 1024);
        if max == 0 {
            return Err("max_output_bytes must be greater than zero".into());
        }
        let wait = WaitOptions::from_request(req.yield_time_ms, req.idle_ms, req.until.as_deref())
            .map_err(|e| format!("invalid until pattern: {e}"))?;
        self.manager
            .read(&req.session_id, req.cursor, max, req.mode, &wait, &cancel)
            .await
            .map(response)
            .map_err(|e| e.to_string())
    }
    #[tool(
        name = "write",
        description = "Write UTF-8 text or named keys through the single writer. Keys use the encoding the application currently expects (for example SS3 arrows in cursor-key mode). Returns confirmed bytes_written and interaction_id, with no terminal output. Success does not prove the input was interpreted as a command. Use read to inspect the result. On partial/uncertain failure do not retry automatically."
    )]
    async fn write(
        &self,
        Parameters(req): Parameters<WriteRequest>,
    ) -> Result<Json<ToolResponse>, String> {
        let entry = self
            .manager
            .get(&req.session_id)
            .map_err(|e| e.to_string())?;
        let bytes = match (req.text, req.keys) {
            (Some(text), None) => text.into_bytes(),
            (None, Some(keys)) => keys::encode_all(&keys, entry.session.application_cursor())
                .map_err(|e| e.to_string())?,
            _ => return Err("provide exactly one of text or keys".into()),
        };
        if bytes.len() > 64 * 1024 {
            return Err("input exceeds 65536 bytes".into());
        }
        let receipt = entry
            .session
            .writer()
            .write(&bytes)
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
        description = "Abandon and force-stop a PTY, then wait up to 3 seconds for it to finish. Idempotent; finished output remains readable without requiring close."
    )]
    async fn close(
        &self,
        Parameters(req): Parameters<CloseRequest>,
    ) -> Result<Json<ToolResponse>, String> {
        self.manager
            .close(&req.session_id)
            .map_err(|e| e.to_string())?;
        let entry = self
            .manager
            .get(&req.session_id)
            .map_err(|e| e.to_string())?;
        // Finalization publishes the termination after draining output (at most 2 seconds).
        let _ = tokio::time::timeout(CLOSE_WAIT, entry.session.wait()).await;
        Ok(response(entry.snapshot()))
    }
}

const DEFAULT_ROWS: u16 = 40;
const DEFAULT_COLS: u16 = 120;
const CLOSE_WAIT: Duration = Duration::from_secs(3);

/// Runs a command line through the user's login shell.
fn shell_command(command: String) -> (String, Vec<String>) {
    #[cfg(not(windows))]
    {
        let shell = std::env::var("SHELL")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "/bin/sh".into());
        (shell, vec!["-lc".into(), command])
    }
    #[cfg(windows)]
    {
        let shell = std::env::var("ComSpec").unwrap_or_else(|_| "cmd.exe".into());
        (shell, vec!["/C".into(), command])
    }
}

/// The instance and port live inside the wait command, so the result omits them.
fn start_result(session_id: &str, snapshot: &Value, launch: Value) -> Value {
    let mut result =
        json!({"session_id": session_id, "state": snapshot["state"], "background_task": launch});
    if !snapshot["termination"].is_null() {
        result["termination"] = snapshot["termination"].clone();
    }
    result
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
    fn start_and_write_accept_their_alternative_inputs() {
        let start: StartRequest =
            serde_json::from_value(json!({"command":"echo hi | wc -c"})).unwrap();
        assert!(start.program.is_none() && start.command.is_some());
        let write: WriteRequest =
            serde_json::from_value(json!({"session_id":"pty_x","keys":["C-c","Up"]})).unwrap();
        assert!(write.text.is_none() && write.keys.unwrap().len() == 2);
    }
    #[cfg(not(windows))]
    #[test]
    fn commands_run_through_a_login_shell() {
        let (shell, args) = shell_command("ls | head".into());
        assert!(!shell.is_empty());
        assert_eq!(args, ["-lc", "ls | head"]);
    }
    #[test]
    fn start_result_carries_only_what_the_model_acts_on() {
        let launch = wait_launch("inst_x", "pty_x", 123).unwrap();
        let running = start_result(
            "pty_x",
            &json!({"state":"running","termination":null}),
            launch.clone(),
        );
        assert_eq!(
            running.as_object().unwrap().keys().collect::<Vec<_>>(),
            ["background_task", "session_id", "state"]
        );
        let exited = start_result(
            "pty_x",
            &json!({"state":"finished","termination":{"reason":"natural_exit","exit_code":0}}),
            launch,
        );
        assert_eq!(exited["termination"]["reason"], "natural_exit");
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
