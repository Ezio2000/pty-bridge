use super::*;
impl PtyWriter {
    pub async fn write(&self, bytes: &[u8]) -> std::result::Result<WriteReceipt, WriteFailure> {
        self.write_with_timeout(bytes, WRITE_TIMEOUT).await
    }

    pub async fn write_with_timeout(
        &self,
        bytes: &[u8],
        timeout: Duration,
    ) -> std::result::Result<WriteReceipt, WriteFailure> {
        let inner = &self.owner.inner;
        let failure = |message: &str| WriteFailure {
            bytes_written: 0,
            delivery_uncertain: false,
            message: message.into(),
        };
        if bytes.is_empty() {
            return Ok(WriteReceipt {
                bytes_written: 0,
                interaction_id: inner.snapshot().activity.interaction_id,
            });
        }
        let progress = Arc::new(AtomicUsize::new(0));
        let (reply, result) = oneshot::channel();
        let input = Input {
            bytes: bytes.to_vec(),
            user: true,
            deadline: Instant::now() + timeout,
            progress: progress.clone(),
            reply: Some(reply),
        };
        {
            let mut state = inner.state.lock().unwrap();
            if state.snapshot.ending || state.snapshot.state != SessionState::Running {
                return Err(failure("session is not writable"));
            }
            state.snapshot.activity.pending_inputs += 1;
        }
        let sent = inner
            .writer
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|sender| sender.try_send(input).is_ok());
        if !sent {
            inner.state.lock().unwrap().snapshot.activity.pending_inputs -= 1;
            return Err(failure("writer queue is full or closed"));
        }
        inner.changed();
        // A finished session never acknowledges input, even while the OS write stays blocked.
        let mut changes = inner.changes.subscribe();
        let finished = async {
            while inner.snapshot().state != SessionState::Finished {
                if changes.changed().await.is_err() {
                    break;
                }
            }
        };
        let outcome = tokio::select! {
            biased;
            outcome = tokio::time::timeout(timeout, result) => outcome,
            () = finished => {
                return Err(WriteFailure {
                    bytes_written: progress.load(Ordering::SeqCst),
                    delivery_uncertain: true,
                    message: "session finished before acknowledging input".into(),
                });
            }
        };
        match outcome {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(WriteFailure {
                bytes_written: progress.load(Ordering::SeqCst),
                delivery_uncertain: true,
                message: "writer stopped before acknowledging input".into(),
            }),
            Err(_) => {
                let _ = inner.stop(
                    FinishReason::WriteTimeout,
                    true,
                    Some("input write deadline exceeded".into()),
                );
                Err(WriteFailure {
                    bytes_written: progress.load(Ordering::SeqCst),
                    delivery_uncertain: true,
                    message: "input write deadline exceeded; do not retry automatically".into(),
                })
            }
        }
    }
}

pub(super) fn writer_loop(
    inner: Arc<Inner>,
    mut writer: Box<dyn Write + Send>,
    inputs: mpsc::Receiver<Input>,
    events: mpsc::Sender<WorkerEvent>,
) {
    while let Ok(input) = inputs.recv() {
        let (seq, cursor) = {
            let mut state = inner.state.lock().unwrap();
            if state.snapshot.ending {
                break;
            }
            state.active_write = Some(input.deadline);
            (state.snapshot.activity.output_seq, state.output.range().1)
        };
        let mut written = 0;
        let result = (|| -> std::io::Result<()> {
            while written < input.bytes.len() {
                if Instant::now() >= input.deadline {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "write deadline exceeded",
                    ));
                }
                if inner.snapshot().ending {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "session is ending",
                    ));
                }
                match writer.write(&input.bytes[written..(written + 4096).min(input.bytes.len())]) {
                    Ok(0) => return Err(std::io::ErrorKind::WriteZero.into()),
                    Ok(n) => {
                        written += n;
                        input.progress.store(written, Ordering::SeqCst);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(error) => return Err(error),
                }
            }
            writer.flush()
        })();
        let receipt = {
            let mut state = inner.state.lock().unwrap();
            state.active_write = None;
            if input.user {
                state.snapshot.activity.pending_inputs -= 1;
            }
            if result.is_ok() && input.user {
                let activity = &mut state.snapshot.activity;
                activity.interaction_id += 1;
                activity.interaction_at = Instant::now();
                activity.interaction_output_seq = seq;
                activity.interaction_cursor = cursor;
                state.snapshot.last_input_at_ms = Some(now_ms());
            }
            WriteReceipt {
                bytes_written: written,
                interaction_id: state.snapshot.activity.interaction_id,
            }
        };
        inner.changed();
        let failed = result.is_err();
        let result = result.map(|()| receipt).map_err(|error| {
            let reason = if error.kind() == std::io::ErrorKind::TimedOut {
                FinishReason::WriteTimeout
            } else {
                FinishReason::WriterFailed
            };
            let _ = inner.stop(reason, true, Some(error.to_string()));
            WriteFailure {
                bytes_written: written,
                delivery_uncertain: false,
                message: error.to_string(),
            }
        });
        if let Some(reply) = input.reply {
            let _ = reply.send(result);
        }
        if failed {
            break;
        }
    }
    drop(writer);
    let _ = events.send(WorkerEvent::WriterEnded);
}
