//! Native PTY sessions with independent input, output and lifecycle handles.
pub mod buffer;
pub mod platform;
mod session;
mod terminal;
mod types;
pub use session::{PtyReader, PtyWriter, Session};
pub use types::*;
