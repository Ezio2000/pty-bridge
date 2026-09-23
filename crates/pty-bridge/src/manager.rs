use crate::{
    MAX_SESSIONS,
    mcp::ReadMode,
    protocol::{receive, send},
    runtime::{self, Ownership},
    silence::{Candidate, Observation},
};
use anyhow::{Context, Result, bail};
use pty_core::{FinishReason, Session, SessionState, StartSpec, Termination};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, VecDeque},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncReadExt, BufReader},
    net::{TcpListener, TcpStream},
    sync::{Mutex as AsyncMutex, OwnedSemaphorePermit, Semaphore},
};

const MONITOR_READY_TIMEOUT: Duration = Duration::from_secs(5);
const RESPONSE_ACK_WINDOW: Duration = Duration::from_secs(2);

struct Receipt {
    id: String,
    start: u64,
    end: u64,
    notice: Option<Candidate>,
    at: Instant,
}
pub struct Entry {
    pub id: String,
    pub host: String,
    pub session: Session,
    spec: StartSpec,
    record: Ownership,
    monitor: Arc<MonitorLink>,
    observation: Mutex<ObservationState>,
    stop_reason: Mutex<Option<(String, FinishReason)>>,
    waiting: AtomicBool,
    _slot: OwnedSemaphorePermit,
}

#[derive(Default)]
struct ObservationState {
    observed: Observation,
    receipts: VecDeque<Receipt>,
}
impl ObservationState {
    fn candidate(&mut self, snapshot: &pty_core::Snapshot) -> Option<Candidate> {
        let candidate = self.observed.candidate(snapshot, Instant::now())?;
        if self
            .receipts
            .iter()
            .filter(|r| r.at.elapsed() < RESPONSE_ACK_WINDOW)
            .any(|r| {
                r.notice
                    .as_ref()
                    .is_some_and(|c| c.interaction_id == candidate.interaction_id)
                    || (candidate.start_cursor < candidate.end_cursor
                        && r.start <= candidate.start_cursor
                        && r.end >= candidate.end_cursor)
            })
        {
            return None;
        }
        Some(candidate)
    }
}
impl Entry {
    fn candidate(&self) -> Option<Candidate> {
        self.observation
            .lock()
            .unwrap()
            .candidate(&self.session.snapshot())
    }
    fn claim(&self, candidate: &Candidate) -> bool {
        let mut observation = self.observation.lock().unwrap();
        let snapshot = self.session.snapshot();
        if observation.candidate(&snapshot).as_ref() != Some(candidate) {
            return false;
        }
        observation
            .observed
            .claim(&snapshot, candidate, Instant::now())
    }
    fn stop(&self, reason: &str, core: FinishReason, force: bool) -> Result<()> {
        if self.session.snapshot().state == SessionState::Finished {
            return Ok(());
        }
        let mut stored = self.stop_reason.lock().unwrap();
        if stored.is_none() {
            *stored = Some((reason.into(), core));
        }
        drop(stored);
        self.session.stop(core, force)
    }
    pub fn termination(&self, termination: Termination) -> Value {
        let core_reason = termination.reason;
        let mut value = serde_json::to_value(termination).unwrap();
        if let Some((reason, requested)) = self.stop_reason.lock().unwrap().as_ref()
            && core_reason == *requested
        {
            value["reason"] = json!(reason);
        }
        value
    }
    pub fn snapshot(&self) -> Value {
        let s = self.session.snapshot();
        json!({"session_id":self.id,"program":self.spec.program,"args":self.spec.args,"cwd":self.spec.cwd,
            "rows":s.rows,"cols":s.cols,"state":s.state,"ending":s.ending,
            "termination":s.termination.map(|t|self.termination(t)),"created_at_ms":s.created_at_ms,
            "last_output_at_ms":s.last_output_at_ms,"last_input_at_ms":s.last_input_at_ms,
            "retained_start":s.retained_start,"retained_end":s.retained_end,
            "interaction_id":s.activity.interaction_id})
    }
}

struct MonitorLink {
    alive: AtomicBool,
}
pub struct Manager {
    instance: String,
    port: u16,
    sessions: Mutex<HashMap<String, Arc<Entry>>>,
    slots: Arc<Semaphore>,
    monitors: AsyncMutex<HashMap<String, Arc<MonitorLink>>>,
    shutting_down: AtomicBool,
    listener_abort: Mutex<Option<tokio::task::AbortHandle>>,
}

impl Manager {
    pub async fn new() -> Result<Arc<Self>> {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
        let manager = Arc::new(Self {
            instance: format!("inst_{}", uuid::Uuid::new_v4().simple()),
            port: listener.local_addr()?.port(),
            sessions: Mutex::new(HashMap::new()),
            slots: Arc::new(Semaphore::new(MAX_SESSIONS)),
            monitors: AsyncMutex::new(HashMap::new()),
            shutting_down: AtomicBool::new(false),
            listener_abort: Mutex::new(None),
        });
        let weak = Arc::downgrade(&manager);
        let listener_task = tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                let Some(manager) = weak.upgrade() else {
                    break;
                };
                tokio::spawn(async move {
                    if let Err(e) = manager.serve(socket).await {
                        tracing::debug!("control connection: {e:#}");
                    }
                });
            }
        });
        *manager.listener_abort.lock().unwrap() = Some(listener_task.abort_handle());
        Ok(manager)
    }
    pub fn instance_id(&self) -> &str {
        &self.instance
    }
    pub fn port(&self) -> u16 {
        self.port
    }
    pub fn get(&self, id: &str) -> Result<Arc<Entry>> {
        self.sessions
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .context("unknown PTY session")
    }

    async fn connect_monitor(self: &Arc<Self>, host: &str) -> Result<Arc<MonitorLink>> {
        runtime::validate_id(host)?;
        let mut monitors = self.monitors.lock().await;
        if let Some(link) = monitors.get(host)
            && link.alive.load(Ordering::SeqCst)
        {
            return Ok(link.clone());
        }
        let connect = async {
            loop {
                if let Ok(record) = runtime::read_monitor(host)
                    && let Ok(socket) = TcpStream::connect(("127.0.0.1", record.port)).await
                {
                    let mut stream = BufReader::new(socket);
                    send(stream.get_mut(),&json!({"action":"register","host_session_id":host,"generation":record.generation,"instance_id":self.instance})).await?;
                    let ack: Value = receive(&mut stream).await?;
                    if ack["ok"] != true || ack["generation"] != record.generation {
                        bail!("monitor handshake rejected");
                    }
                    return Ok::<_, anyhow::Error>(stream);
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        };
        let stream = tokio::time::timeout(MONITOR_READY_TIMEOUT, connect)
            .await
            .context("monitor not ready within 5 seconds; no PTY was started")??;
        let link = Arc::new(MonitorLink {
            alive: AtomicBool::new(true),
        });
        monitors.insert(host.into(), link.clone());
        let weak = Arc::downgrade(self);
        let host = host.to_string();
        let connection = link.clone();
        tokio::spawn(async move {
            let result = monitor_loop(weak.clone(), &host, stream).await;
            connection.alive.store(false, Ordering::SeqCst);
            if let Some(manager) = weak.upgrade() {
                for entry in manager
                    .sessions
                    .lock()
                    .unwrap()
                    .values()
                    .filter(|entry| Arc::ptr_eq(&entry.monitor, &connection))
                {
                    let _ = entry.stop(
                        "monitor_disconnected",
                        FinishReason::OwnerDisconnected,
                        true,
                    );
                }
            }
            if let Err(e) = result {
                tracing::debug!("monitor disconnected: {e:#}");
            }
        });
        Ok(link)
    }

    pub async fn start(self: &Arc<Self>, host: &str, spec: StartSpec) -> Result<Arc<Entry>> {
        if self.shutting_down.load(Ordering::SeqCst) {
            bail!("PTY server is shutting down");
        }
        let link = self.connect_monitor(host).await?;
        let slot = {
            let mut sessions = self.sessions.lock().unwrap();
            if self.slots.available_permits() == 0 {
                let oldest = sessions
                    .iter()
                    .filter(|(_, e)| {
                        e.session.snapshot().state == SessionState::Finished
                            && !e.waiting.load(Ordering::SeqCst)
                    })
                    .min_by_key(|(_, e)| e.session.snapshot().created_at_ms)
                    .map(|(id, _)| id.clone());
                if let Some(id) = oldest {
                    sessions.remove(&id);
                }
            }
            self.slots
                .clone()
                .try_acquire_owned()
                .context("PTY session limit reached")?
        };
        let start_spec = spec.clone();
        let session = tokio::task::spawn_blocking(move || Session::start(start_spec)).await??;
        let id = format!("pty_{}", uuid::Uuid::new_v4().simple());
        let record = Ownership {
            host_session_id: host.into(),
            instance_id: self.instance.clone(),
            session_id: id.clone(),
            port: self.port,
            process: session.process_locator(),
        };
        let entry = Arc::new(Entry {
            id: id.clone(),
            host: host.into(),
            session,
            spec,
            record,
            monitor: link.clone(),
            observation: Mutex::new(ObservationState::default()),
            stop_reason: Mutex::new(None),
            waiting: AtomicBool::new(false),
            _slot: slot,
        });
        {
            let mut sessions = self.sessions.lock().unwrap();
            if !link.alive.load(Ordering::SeqCst) || self.shutting_down.load(Ordering::SeqCst) {
                entry.stop(
                    "monitor_disconnected",
                    FinishReason::OwnerDisconnected,
                    true,
                )?;
                bail!("monitor disconnected during PTY creation");
            }
            runtime::write_ownership(&entry.record)?;
            sessions.insert(id, entry.clone());
        }
        // No manager reference is held by the finalizer, so dropping the MCP owner can clean up.
        let finalizer = entry.clone();
        tokio::spawn(async move {
            finalizer.session.wait().await;
            runtime::remove_ownership(&finalizer.record);
        });
        Ok(entry)
    }

    pub fn snapshots(&self, id: Option<&str>) -> Result<Vec<Value>> {
        if let Some(id) = id {
            return Ok(vec![self.get(id)?.snapshot()]);
        }
        let sessions = self.sessions.lock().unwrap();
        let mut entries: Vec<_> = sessions.values().collect();
        entries.sort_by_key(|e| e.session.snapshot().created_at_ms);
        Ok(entries.into_iter().map(|e| e.snapshot()).collect())
    }
    pub fn close(&self, id: &str) -> Result<()> {
        self.get(id)?
            .stop("explicit_close", FinishReason::ExplicitClose, true)
    }
    pub fn signal(&self, id: &str, force: bool) -> Result<()> {
        self.get(id)?.stop(
            if force { "killed" } else { "terminated" },
            if force {
                FinishReason::Killed
            } else {
                FinishReason::Terminated
            },
            force,
        )
    }
    pub fn finish_host(&self, host: &str, reason: &str, core: FinishReason) {
        for entry in self
            .sessions
            .lock()
            .unwrap()
            .values()
            .filter(|e| e.host == host)
        {
            if let Err(e) = entry.stop(reason, core, true) {
                tracing::warn!("stop PTY: {e:#}");
            }
        }
    }
    pub fn shutdown(&self) {
        self.shutting_down.store(true, Ordering::SeqCst);
        for entry in self.sessions.lock().unwrap().values() {
            let _ = entry.stop("server_shutdown", FinishReason::Shutdown, true);
        }
    }

    pub async fn shutdown_and_wait(&self) -> Result<()> {
        self.shutdown();
        let entries: Vec<_> = self.sessions.lock().unwrap().values().cloned().collect();
        tokio::time::timeout(Duration::from_secs(4), async {
            for entry in entries {
                entry.session.wait().await;
                runtime::remove_ownership(&entry.record);
            }
        })
        .await
        .context("PTY shutdown did not finish within 4 seconds")?;
        Ok(())
    }

    pub async fn read(
        &self,
        id: &str,
        cursor: u64,
        max: usize,
        wait: Duration,
        mode: ReadMode,
    ) -> Result<Value> {
        use base64::{Engine as _, engine::general_purpose::STANDARD};
        let entry = self.get(id)?;
        let reader = entry.session.reader();
        reader.wait_output(cursor, wait).await;
        let screen = matches!(mode, ReadMode::Auto | ReadMode::Screen).then(|| reader.screen());
        let mode = match (mode, &screen) {
            (ReadMode::Auto, Some(s)) if s.alternate_screen => ReadMode::Screen,
            (ReadMode::Auto, _) => ReadMode::Text,
            (mode, _) => mode,
        };
        // A screen reflects every byte before its end, so its receipt covers them all.
        let (receipt_start, start, end, dropped, body) = match mode {
            ReadMode::Screen => {
                let screen = screen.expect("screen captured for screen mode");
                let end = screen.end_cursor;
                (0, cursor.min(end), end, 0, json!({"screen": screen}))
            }
            ReadMode::Raw => {
                let data = reader.read(cursor, max);
                let text = std::str::from_utf8(&data.bytes);
                let mut output = json!({"text": String::from_utf8_lossy(&data.bytes), "text_lossy": text.is_err()});
                if text.is_err() {
                    output["base64"] = json!(STANDARD.encode(&data.bytes));
                }
                (
                    data.start_cursor,
                    data.start_cursor,
                    data.next_cursor,
                    data.dropped_bytes,
                    json!({"output": output}),
                )
            }
            _ => {
                let data = reader.read_text(cursor, max);
                (
                    data.start_cursor,
                    data.start_cursor,
                    data.next_cursor,
                    data.dropped_bytes,
                    json!({"output": {"text": data.text, "rows_dropped": data.rows_dropped}}),
                )
            }
        };
        let receipt = format!("read_{}", uuid::Uuid::new_v4().simple());
        let notice = {
            let mut observation = entry.observation.lock().unwrap();
            let notice = observation.candidate(&entry.session.snapshot());
            observation.receipts.push_back(Receipt {
                id: receipt.clone(),
                start: receipt_start,
                end,
                notice: notice.clone(),
                at: Instant::now(),
            });
            while observation.receipts.len() > 128 {
                observation.receipts.pop_front();
            }
            notice
        };
        let snapshot = entry.snapshot();
        let mut result = json!({"instance_id":self.instance,"control_port":self.port,"session_id":id,"receipt_id":receipt,
            "mode":mode,"start_cursor":start,"next_cursor":end,"dropped_bytes":dropped,
            "state":snapshot["state"],"termination":snapshot["termination"],
            "silence":notice.map(|candidate|json!({"candidate":candidate,"message":format!("PTY {id} 疑似停滞，请调用 read 检查。")}))});
        result
            .as_object_mut()
            .unwrap()
            .extend(body.as_object().unwrap().clone());
        Ok(result)
    }

    pub fn observe(&self, id: &str, receipt: &str) -> Result<()> {
        let entry = self.get(id)?;
        let mut observation = entry.observation.lock().unwrap();
        let receipt = observation
            .receipts
            .iter()
            .position(|r| r.id == receipt)
            .and_then(|index| observation.receipts.remove(index));
        if let Some(receipt) = receipt {
            observation.observed.read(receipt.start, receipt.end);
            if let Some(notice) = receipt.notice {
                observation.observed.notice_delivered(notice.interaction_id);
            }
        }
        Ok(())
    }

    async fn serve(self: Arc<Self>, socket: TcpStream) -> Result<()> {
        let mut stream = BufReader::new(socket);
        let request: Value =
            tokio::time::timeout(Duration::from_secs(5), receive(&mut stream)).await??;
        let result=async {
            if request["instance_id"]!=self.instance {bail!("PTY instance mismatch");}
            let id=request["session_id"].as_str().context("missing session id")?;
            match request["action"].as_str() {
                Some("wait")=> {
                    let entry=self.get(id)?;
                    if entry.waiting.swap(true,Ordering::SeqCst) {bail!("a bgshell already waits for this PTY");}
                    struct WaitGuard(Arc<Entry>);
                    impl Drop for WaitGuard {fn drop(&mut self) {self.0.waiting.store(false,Ordering::SeqCst);let _=self.0.stop("bgshell_disconnected",FinishReason::OwnerDisconnected,true);}}
                    let _guard=WaitGuard(entry.clone());
                    send(stream.get_mut(),&json!({"ok":true,"state":entry.session.snapshot().state})).await?;
                    let mut byte=[0];
                    tokio::select! {
                        terminal=entry.session.wait()=>send(stream.get_mut(),&json!({"termination":entry.termination(terminal)})).await?,
                        _=stream.read(&mut byte)=>bail!("bgshell disconnected before PTY finished"),
                    }
                    return Ok::<_,anyhow::Error>(());
                }
                Some("observe")=>self.observe(id,request["receipt_id"].as_str().context("missing receipt id")?)?,
                Some("finish_owned")=>self.get(id)?.stop("host_session_ended",FinishReason::OwnerEnded,true)?,
                _=>bail!("unknown control request"),
            }
            send(stream.get_mut(),&json!({"ok":true})).await?;Ok(())
        }.await;
        if let Err(error) = result {
            let _ = send(
                stream.get_mut(),
                &json!({"ok":false,"error":error.to_string()}),
            )
            .await;
            return Err(error);
        }
        Ok(())
    }
}
impl Drop for Manager {
    fn drop(&mut self) {
        self.shutdown();
        if let Some(listener) = self.listener_abort.lock().unwrap().take() {
            listener.abort();
        }
    }
}

async fn monitor_loop(
    manager: std::sync::Weak<Manager>,
    host: &str,
    mut stream: BufReader<TcpStream>,
) -> Result<()> {
    let mut interval = tokio::time::interval(Duration::from_millis(250));
    loop {
        interval.tick().await;
        let Some(owner) = manager.upgrade() else {
            return Ok(());
        };
        if owner.shutting_down.load(Ordering::SeqCst) {
            return Ok(());
        }
        let entries: Vec<_> = owner
            .sessions
            .lock()
            .unwrap()
            .values()
            .filter(|e| e.host == host)
            .cloned()
            .collect();
        drop(owner);
        tokio::time::timeout(Duration::from_secs(2), async {
            send(stream.get_mut(), &json!({"action":"ping"})).await?;
            let response: Value = receive(&mut stream).await?;
            if response["ok"] != true {
                bail!("monitor heartbeat rejected");
            }
            for entry in entries {
                if let Some(candidate) = entry.candidate() {
                    send(
                        stream.get_mut(),
                        &json!({"action":"offer","session_id":entry.id,"candidate":candidate}),
                    )
                    .await?;
                    let claim: Value = receive(&mut stream).await?;
                    if claim["action"] != "claim"
                        || claim["session_id"] != entry.id
                        || claim["candidate"] != serde_json::to_value(&candidate)?
                    {
                        bail!("invalid monitor claim");
                    }
                    let notify = entry.claim(&candidate);
                    send(stream.get_mut(), &json!({"notify":notify})).await?;
                    let response: Value = receive(&mut stream).await?;
                    if response["ok"] != true {
                        bail!("monitor notification failed");
                    }
                }
            }
            Ok::<_, anyhow::Error>(())
        })
        .await
        .context("monitor heartbeat timed out")??;
    }
}
