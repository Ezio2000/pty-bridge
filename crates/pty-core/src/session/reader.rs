use super::*;
use crate::render::{self, ScreenSnapshot, TextRead};
impl PtyReader {
    pub fn read(&self, cursor: u64, max_bytes: usize) -> BufferRead {
        self.owner
            .inner
            .state
            .lock()
            .unwrap()
            .output
            .read(cursor, max_bytes)
    }
    /// The emulated screen and the cursor of the first byte it has not applied.
    pub fn screen(&self) -> ScreenSnapshot {
        let state = self.owner.inner.state.lock().unwrap();
        render::snapshot(state.screen.screen(), state.output.range().1)
    }
    /// Renders retained bytes from `cursor` as plain text at the session width.
    pub fn read_text(&self, cursor: u64, max_bytes: usize) -> TextRead {
        let (mut data, rows, cols, end) = {
            let state = self.owner.inner.state.lock().unwrap();
            let s = &state.snapshot;
            (
                state.output.read(cursor, max_bytes),
                s.rows,
                s.cols,
                state.output.range().1,
            )
        };
        // A truncated range ends at a line boundary so the next one starts outside
        // escape sequences and multibyte characters.
        if data.next_cursor < end
            && let Some(last) = data.bytes.iter().rposition(|b| *b == b'\n')
        {
            data.bytes.truncate(last + 1);
            data.next_cursor = data.start_cursor + last as u64 + 1;
        }
        let (text, rows_dropped) = render::render_text(&data.bytes, rows, cols);
        TextRead {
            text,
            start_cursor: data.start_cursor,
            next_cursor: data.next_cursor,
            dropped_bytes: data.dropped_bytes,
            rows_dropped,
        }
    }
    /// Waits until output exists at or beyond `cursor`, the session finishes, or `timeout`.
    pub async fn wait_output(&self, cursor: u64, timeout: Duration) {
        let mut changes = self.owner.inner.changes.subscribe();
        let _ = tokio::time::timeout(timeout, async {
            loop {
                let state = self.owner.inner.snapshot();
                if state.retained_end > cursor || state.state == SessionState::Finished {
                    break;
                }
                if changes.changed().await.is_err() {
                    break;
                }
            }
        })
        .await;
    }
    pub async fn read_wait(&self, cursor: u64, max_bytes: usize, timeout: Duration) -> BufferRead {
        self.wait_output(cursor, timeout).await;
        self.read(cursor, max_bytes)
    }
}

pub(super) fn reader_loop(
    inner: Arc<Inner>,
    mut reader: Box<dyn Read + Send>,
    inputs: mpsc::SyncSender<Input>,
    events: mpsc::Sender<WorkerEvent>,
) {
    let mut bytes = [0u8; 8192];
    let mut protocol = terminal::TerminalProtocol::default();
    let failure = loop {
        match reader.read(&mut bytes) {
            Ok(0) => break None,
            Ok(count) => {
                // Visible bytes reach the screen before a later query is answered.
                let mut response = Vec::new();
                let mut activity = true;
                for chunk in protocol.process(&bytes[..count]) {
                    match chunk {
                        terminal::Chunk::Visible(visible) => {
                            inner.record(activity, &visible);
                            activity = false;
                        }
                        terminal::Chunk::Query(query) => response.extend(inner.answer(query)),
                    }
                }
                if activity {
                    inner.record(true, &[]);
                }
                if !response.is_empty() && !inner.snapshot().ending {
                    let input = Input {
                        bytes: response,
                        user: false,
                        deadline: Instant::now() + WRITE_TIMEOUT,
                        progress: Arc::new(AtomicUsize::new(0)),
                        reply: None,
                    };
                    if inputs.try_send(input).is_err() {
                        break Some("terminal response queue unavailable".into());
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            // Unix PTYs commonly report EIO when the last slave closes.
            #[cfg(unix)]
            Err(error) if error.raw_os_error() == Some(libc::EIO) => break None,
            Err(error) => break Some(error.to_string()),
        }
    };
    inner.record(false, &protocol.finish());
    drop(reader);
    drop(inputs);
    let _ = events.send(WorkerEvent::ReaderEnded(failure));
}
