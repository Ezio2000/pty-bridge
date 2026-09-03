pub mod background_task;
pub mod buffer;
pub mod hooks;
pub mod manager;
pub mod mcp;
pub mod runtime;

pub const APP_NAME: &str = "pty-bridge";
pub const DEFAULT_BACKGROUND_TASK_TICKET_TTL_SECONDS: u64 = 30 * 60;
pub const DEFAULT_ROWS: u16 = 24;
pub const DEFAULT_COLS: u16 = 80;
pub const MAX_SESSIONS: usize = 64;
pub const OUTPUT_CAPACITY: usize = 1024 * 1024;
