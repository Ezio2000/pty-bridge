use std::{
    io::{Read, Write},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::sync::{oneshot, watch};

use crate::buffer::{BufferRead, OutputBuffer};
use crate::platform::{Process, ProcessLocator};
use crate::terminal;
use crate::types::*;
use anyhow::{Context, Result, bail};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
mod reader;
mod writer;
use reader::reader_loop;
use writer::writer_loop;
struct State {
    snapshot: Snapshot,
    output: OutputBuffer,
    /// Emulated screen, advanced under the same lock as `output` so both share one cursor.
    screen: vt100::Parser,
    requested: Option<Termination>,
    active_write: Option<Instant>,
}

struct Inner {
    state: Mutex<State>,
    master: Mutex<Option<Box<dyn MasterPty + Send>>>,
    process: Process,
    writer: Mutex<Option<mpsc::SyncSender<Input>>>,
    worker_events: mpsc::Sender<WorkerEvent>,
    changes: watch::Sender<u64>,
}

struct Owner {
    inner: Arc<Inner>,
}
impl Drop for Owner {
    fn drop(&mut self) {
        let _ = self.inner.stop(FinishReason::Shutdown, true, None);
    }
}

#[derive(Clone)]
pub struct Session {
    owner: Arc<Owner>,
}
#[derive(Clone)]
pub struct PtyWriter {
    owner: Arc<Owner>,
}
#[derive(Clone)]
pub struct PtyReader {
    owner: Arc<Owner>,
}

struct Input {
    bytes: Vec<u8>,
    user: bool,
    deadline: Instant,
    progress: Arc<AtomicUsize>,
    reply: Option<oneshot::Sender<std::result::Result<WriteReceipt, WriteFailure>>>,
}

enum WorkerEvent {
    ReaderEnded(Option<String>),
    WriterEnded,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

impl Inner {
    fn changed(&self) {
        self.changes.send_modify(|version| *version += 1);
    }

    fn snapshot(&self) -> Snapshot {
        let state = self.state.lock().unwrap();
        let mut snapshot = state.snapshot.clone();
        (snapshot.retained_start, snapshot.retained_end) = state.output.range();
        snapshot
    }

    fn stop(&self, reason: FinishReason, force: bool, message: Option<String>) -> Result<()> {
        {
            let mut state = self.state.lock().unwrap();
            if state.snapshot.state == SessionState::Finished
                || (state.snapshot.ending && state.requested.is_none())
            {
                return Ok(());
            }
            if state.requested.is_none() {
                state.requested = Some(Termination {
                    reason,
                    exit_code: None,
                    message,
                });
            }
            state.snapshot.ending = true;
        }
        self.changed();
        self.process.signal(force)
    }

    fn record(&self, raw_activity: bool, bytes: &[u8]) {
        {
            let mut state = self.state.lock().unwrap();
            if raw_activity {
                state.snapshot.activity.output_seq += 1;
                state.snapshot.activity.last_output_at = Some(Instant::now());
                state.snapshot.last_output_at_ms = Some(now_ms());
            }
            state.output.append(bytes);
            state.screen.process(bytes);
        }
        self.changed();
    }

    fn answer(&self, query: terminal::Query) -> Vec<u8> {
        query.answer(self.state.lock().unwrap().screen.screen())
    }

    fn shutdown_io(&self) {
        self.writer.lock().unwrap().take();
        // Drop ConPTY outside the state lock while the reader continues draining.
        let master = self.master.lock().unwrap().take();
        drop(master);
    }
}

impl Session {
    /// Returns only after a real child and all supervising workers were created.
    pub fn start(spec: StartSpec) -> Result<Self> {
        if spec.program.is_empty() || spec.rows == 0 || spec.cols == 0 {
            bail!("program and nonzero terminal dimensions are required");
        }
        let pair = native_pty_system().openpty(PtySize {
            rows: spec.rows,
            cols: spec.cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        let mut command = CommandBuilder::new(&spec.program);
        command.args(&spec.args);
        command.cwd(&spec.cwd);
        for (key, value) in &spec.env {
            command.env(key, value);
        }
        let mut child = pair
            .slave
            .spawn_command(command)
            .context("spawn PTY child")?;
        drop(pair.slave);
        let setup = (|| -> Result<_> {
            let process = Process::new(child.as_ref(), pair.master.as_ref())?;
            let reader = pair.master.try_clone_reader()?;
            let writer = pair.master.take_writer()?;
            Ok((process, reader, writer))
        })();
        let (process, reader, writer) = match setup {
            Ok(setup) => setup,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
        };
        let (input_tx, input_rx) = mpsc::sync_channel(64);
        let (event_tx, event_rx) = mpsc::channel();
        let (changes, _) = watch::channel(0);
        let inner = Arc::new(Inner {
            state: Mutex::new(State {
                snapshot: Snapshot {
                    state: SessionState::Starting,
                    termination: None,
                    rows: spec.rows,
                    cols: spec.cols,
                    created_at_ms: now_ms(),
                    last_output_at_ms: None,
                    last_input_at_ms: None,
                    retained_start: 0,
                    retained_end: 0,
                    ending: false,
                    activity: Activity {
                        interaction_id: 1,
                        interaction_at: Instant::now(),
                        interaction_output_seq: 0,
                        interaction_cursor: 0,
                        output_seq: 0,
                        last_output_at: None,
                        pending_inputs: 0,
                    },
                },
                output: OutputBuffer::new(OUTPUT_CAPACITY),
                screen: vt100::Parser::new(spec.rows, spec.cols, 0),
                requested: None,
                active_write: None,
            }),
            master: Mutex::new(Some(pair.master)),
            process,
            writer: Mutex::new(Some(input_tx.clone())),
            worker_events: event_tx.clone(),
            changes,
        });
        let owner = Arc::new(Owner {
            inner: inner.clone(),
        });
        // A failed thread creation still kills and reaps the child.
        let workers = (|| -> Result<()> {
            let state = inner.clone();
            let events = inner.worker_events.clone();
            std::thread::Builder::new()
                .name("pty-writer".into())
                .spawn(move || writer_loop(state, writer, input_rx, events))?;
            let state = inner.clone();
            let events = inner.worker_events.clone();
            std::thread::Builder::new()
                .name("pty-reader".into())
                .spawn(move || reader_loop(state, reader, input_tx, events))?;
            Ok(())
        })();
        if let Err(error) = workers {
            let _ = inner.stop(FinishReason::Shutdown, true, None);
            inner.shutdown_io();
            let _ = child.wait();
            return Err(error);
        }
        inner.state.lock().unwrap().snapshot.state = SessionState::Running;
        let state = inner.clone();
        // Keep a fallback reaper if spawning the supervisor itself fails.
        let child_slot = Arc::new(Mutex::new(Some(child)));
        let slot = child_slot.clone();
        if let Err(error) = std::thread::Builder::new()
            .name("pty-supervisor".into())
            .spawn(move || {
                let child = slot.lock().unwrap().take().unwrap();
                supervise(state, child, event_rx);
            })
        {
            let _ = inner.stop(FinishReason::Shutdown, true, None);
            inner.shutdown_io();
            if let Some(mut child) = child_slot.lock().unwrap().take() {
                let _ = child.wait();
            }
            return Err(error.into());
        }
        Ok(Self { owner })
    }

    pub fn writer(&self) -> PtyWriter {
        PtyWriter {
            owner: self.owner.clone(),
        }
    }
    pub fn reader(&self) -> PtyReader {
        PtyReader {
            owner: self.owner.clone(),
        }
    }
    pub fn snapshot(&self) -> Snapshot {
        self.owner.inner.snapshot()
    }
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.owner.inner.changes.subscribe()
    }
    /// Whether the application enabled cursor-key mode, which changes arrow key encoding.
    pub fn application_cursor(&self) -> bool {
        let state = self.owner.inner.state.lock().unwrap();
        state.screen.screen().application_cursor()
    }
    pub fn process_locator(&self) -> ProcessLocator {
        self.owner.inner.process.locator()
    }
    pub fn close(&self) -> Result<()> {
        self.stop(FinishReason::ExplicitClose, true)
    }
    pub fn stop(&self, reason: FinishReason, force: bool) -> Result<()> {
        self.owner.inner.stop(reason, force, None)
    }

    pub fn resize(&self, rows: u16, cols: u16) -> Result<()> {
        if rows == 0 || cols == 0 {
            bail!("rows and cols must be greater than zero");
        }
        let inner = &self.owner.inner;
        if inner.snapshot().ending {
            bail!("session is ending");
        }
        inner
            .master
            .lock()
            .unwrap()
            .as_ref()
            .context("session is finished")?
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })?;
        {
            let mut state = inner.state.lock().unwrap();
            state.snapshot.rows = rows;
            state.snapshot.cols = cols;
            state.screen.screen_mut().set_size(rows, cols);
        }
        inner.changed();
        Ok(())
    }

    pub async fn wait(&self) -> Termination {
        let mut changes = self.subscribe();
        loop {
            if let Some(termination) = self.snapshot().termination {
                return termination;
            }
            let _ = changes.changed().await;
        }
    }
}

fn supervise(
    inner: Arc<Inner>,
    mut child: Box<dyn Child + Send + Sync>,
    events: mpsc::Receiver<WorkerEvent>,
) {
    let mut child_exit = None;
    let mut exit_at = None;
    let mut reader_end: Option<(Instant, Option<String>)> = None;
    let mut writer_done = false;
    loop {
        match events.recv_timeout(Duration::from_millis(10)) {
            Ok(WorkerEvent::ReaderEnded(error)) => {
                reader_end = Some((Instant::now(), error));
            }
            Ok(WorkerEvent::WriterEnded) => writer_done = true,
            _ => {}
        }
        if exit_at.is_none() {
            match child.try_wait() {
                Ok(Some(exit)) => {
                    child_exit = Some(exit.exit_code());
                    exit_at = Some(Instant::now());
                    inner.state.lock().unwrap().snapshot.ending = true;
                    // Descendants cannot keep a finalized session's slave open.
                    let _ = inner.process.signal(true);
                    inner.shutdown_io();
                    inner.changed();
                }
                Err(error) => {
                    let _ = inner.stop(FinishReason::WaitFailed, true, Some(error.to_string()));
                    let _ = child.wait();
                    exit_at = Some(Instant::now());
                    inner.shutdown_io();
                }
                Ok(None) => {}
            }
        }
        let expired = inner
            .state
            .lock()
            .unwrap()
            .active_write
            .is_some_and(|deadline| Instant::now() >= deadline);
        if expired {
            let _ = inner.stop(
                FinishReason::WriteTimeout,
                true,
                Some("writer deadline exceeded".into()),
            );
        }
        if exit_at.is_none()
            && let Some((ended, error)) = &reader_end
            && (error.is_some() || ended.elapsed() >= Duration::from_millis(100))
        {
            let _ = inner.stop(
                FinishReason::ReaderFailed,
                true,
                Some(
                    error
                        .clone()
                        .unwrap_or_else(|| "output channel closed while child is alive".into()),
                ),
            );
        }
        if let Some(exited) = exit_at
            && ((reader_end.is_some() && writer_done) || exited.elapsed() >= DRAIN_TIMEOUT)
        {
            let mut state = inner.state.lock().unwrap();
            let drain_error = if reader_end.is_none() || !writer_done {
                Some("PTY workers exceeded drain deadline".to_string())
            } else {
                reader_end.as_ref().and_then(|(_, error)| error.clone())
            };
            let mut termination = state.requested.take().unwrap_or(Termination {
                reason: if drain_error.is_some() {
                    FinishReason::ReaderFailed
                } else {
                    FinishReason::NaturalExit
                },
                exit_code: child_exit,
                message: drain_error,
            });
            termination.exit_code = child_exit;
            state.snapshot.state = SessionState::Finished;
            state.snapshot.termination = Some(termination);
            drop(state);
            inner.changed();
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn sleeper() -> Session {
        #[cfg(unix)]
        let spec = StartSpec::new(
            "/bin/sh",
            vec!["-c".into(), "sleep 30".into()],
            std::env::current_dir().unwrap(),
        );
        #[cfg(windows)]
        let spec = StartSpec::new(
            "cmd.exe",
            vec!["/C".into(), "ping -n 30 127.0.0.1 >NUL".into()],
            std::env::current_dir().unwrap(),
        );
        Session::start(spec).unwrap()
    }
    #[tokio::test]
    async fn reader_error_ends_live_child_immediately() {
        struct FailedReader;
        impl Read for FailedReader {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("injected read failure"))
            }
        }
        let session = sleeper();
        let inner = session.owner.inner.clone();
        let input = inner.writer.lock().unwrap().as_ref().unwrap().clone();
        reader_loop(
            inner.clone(),
            Box::new(FailedReader),
            input,
            inner.worker_events.clone(),
        );
        let result = tokio::time::timeout(Duration::from_secs(3), session.wait())
            .await
            .unwrap();
        assert_eq!(result.reason, FinishReason::ReaderFailed);
        assert!(result.message.unwrap().contains("injected"));
    }
    #[tokio::test]
    async fn partial_write_failure_reports_confirmed_bytes_and_stops_child() {
        struct FailedWriter {
            wrote: bool,
        }
        impl Write for FailedWriter {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                if self.wrote {
                    Err(std::io::Error::other("injected write failure"))
                } else {
                    self.wrote = true;
                    Ok(bytes.len().min(2))
                }
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let session = sleeper();
        let inner = session.owner.inner.clone();
        let (tx, rx) = mpsc::sync_channel(1);
        let (reply, result) = oneshot::channel();
        inner.state.lock().unwrap().snapshot.activity.pending_inputs += 1;
        tx.send(Input {
            bytes: b"abcdef".to_vec(),
            user: true,
            deadline: Instant::now() + WRITE_TIMEOUT,
            progress: Arc::new(AtomicUsize::new(0)),
            reply: Some(reply),
        })
        .unwrap();
        drop(tx);
        writer_loop(
            inner.clone(),
            Box::new(FailedWriter { wrote: false }),
            rx,
            inner.worker_events.clone(),
        );
        let error = result.await.unwrap().unwrap_err();
        assert_eq!(error.bytes_written, 2);
        assert!(!error.delivery_uncertain);
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(3), session.wait())
                .await
                .unwrap()
                .reason,
            FinishReason::WriterFailed
        );
    }
}
