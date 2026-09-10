use super::*;
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
    pub async fn read_wait(&self, cursor: u64, max_bytes: usize, timeout: Duration) -> BufferRead {
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
                let (visible, response) = protocol.process(&bytes[..count]);
                inner.record(true, &visible);
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
