use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    path::PathBuf,
    time::{Duration, Instant},
};
pub const OUTPUT_CAPACITY: usize = 1024 * 1024;
pub const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
pub(crate) const DRAIN_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone)]
pub struct StartSpec {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub env: HashMap<String, String>,
    pub rows: u16,
    pub cols: u16,
}

impl StartSpec {
    pub fn new(program: impl Into<String>, args: Vec<String>, cwd: PathBuf) -> Self {
        Self {
            program: program.into(),
            args,
            cwd,
            env: HashMap::new(),
            rows: 24,
            cols: 80,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    Starting,
    Running,
    Finished,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinishReason {
    NaturalExit,
    ExplicitClose,
    Terminated,
    Killed,
    ReaderFailed,
    WriterFailed,
    WriteTimeout,
    WaitFailed,
    OwnerDisconnected,
    OwnerEnded,
    Shutdown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Termination {
    pub reason: FinishReason,
    pub exit_code: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// Byte activity and input boundaries, independent of any consumer's interpretation.
#[derive(Debug, Clone)]
pub struct Activity {
    pub interaction_id: u64,
    pub interaction_at: Instant,
    pub interaction_output_seq: u64,
    pub interaction_cursor: u64,
    pub output_seq: u64,
    pub last_output_at: Option<Instant>,
    pub pending_inputs: usize,
}

#[derive(Debug, Clone)]
pub struct Snapshot {
    pub state: SessionState,
    pub termination: Option<Termination>,
    pub rows: u16,
    pub cols: u16,
    pub created_at_ms: u64,
    pub last_output_at_ms: Option<u64>,
    pub last_input_at_ms: Option<u64>,
    pub retained_start: u64,
    pub retained_end: u64,
    pub ending: bool,
    pub activity: Activity,
}

#[derive(Debug, Clone, Serialize)]
pub struct WriteReceipt {
    pub bytes_written: usize,
    pub interaction_id: u64,
}

#[derive(Debug, Clone, thiserror::Error, Serialize)]
#[error(
    "{message}; confirmed bytes written={bytes_written}, delivery uncertain={delivery_uncertain}"
)]
pub struct WriteFailure {
    pub bytes_written: usize,
    pub delivery_uncertain: bool,
    pub message: String,
}
