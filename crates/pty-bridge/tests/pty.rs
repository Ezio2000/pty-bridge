use pty_bridge::protocol::{receive, send};
use pty_bridge::{manager::Manager, runtime, wait};
use pty_core::{SessionState, StartSpec};
use serde_json::{Value, json};
use std::{
    io::Write,
    process::{Command, Stdio},
    sync::Arc,
    time::Duration,
};
use tokio::{io::BufReader, net::TcpStream};

struct Monitor {
    host: String,
    child: std::process::Child,
}
impl Monitor {
    async fn start() -> Self {
        let host = format!("host_{}", uuid::Uuid::new_v4().simple());
        let child = Command::new(env!("CARGO_BIN_EXE_pty-bridge"))
            .args(["monitor", "--host-session-id", &host])
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let monitor = Self { host, child };
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while runtime::read_monitor(&monitor.host).is_err() {
            assert!(tokio::time::Instant::now() < deadline);
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        monitor
    }
    fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
impl Drop for Monitor {
    fn drop(&mut self) {
        self.kill();
        let _ = std::fs::remove_file(runtime::monitor_path(&self.host).unwrap());
    }
}
fn command(script: &str) -> StartSpec {
    #[cfg(unix)]
    {
        StartSpec::new(
            "/bin/sh",
            vec!["-c".into(), script.into()],
            std::env::current_dir().unwrap(),
        )
    }
    #[cfg(windows)]
    {
        StartSpec::new(
            "cmd.exe",
            vec!["/C".into(), script.into()],
            std::env::current_dir().unwrap(),
        )
    }
}
fn long_running() -> StartSpec {
    #[cfg(unix)]
    {
        command("printf READY; sleep 30")
    }
    #[cfg(windows)]
    {
        command("echo READY & ping -n 30 127.0.0.1 >NUL")
    }
}
fn marker() -> StartSpec {
    #[cfg(unix)]
    {
        command("printf NATURAL_EXIT_OK")
    }
    #[cfg(windows)]
    {
        command("echo NATURAL_EXIT_OK")
    }
}
async fn finished(entry: &pty_bridge::manager::Entry) {
    tokio::time::timeout(Duration::from_secs(6), entry.session.wait())
        .await
        .unwrap();
}
fn hook(command: &str, input: Value) -> Value {
    let mut child = Command::new(env!("CARGO_BIN_EXE_pty-bridge"))
        .args(["hook", command])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.to_string().as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    if output.stdout.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&output.stdout).unwrap()
    }
}
async fn wait_connection(manager: &Manager, id: &str) -> BufReader<TcpStream> {
    let mut stream = BufReader::new(
        TcpStream::connect(("127.0.0.1", manager.port()))
            .await
            .unwrap(),
    );
    send(
        stream.get_mut(),
        &json!({"action":"wait","instance_id":manager.instance_id(),"session_id":id}),
    )
    .await
    .unwrap();
    let ack: Value = receive(&mut stream).await.unwrap();
    assert_eq!(ack["ok"], true);
    stream
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn start_is_real_and_late_bgshell_replays_terminal_result() {
    let monitor = Monitor::start().await;
    let manager = Manager::new().await.unwrap();
    let entry = manager.start(&monitor.host, marker()).await.unwrap();
    finished(&entry).await;
    assert_eq!(
        wait::run(manager.instance_id(), &entry.id, manager.port())
            .await
            .unwrap(),
        0
    );
    let result = manager
        .read(&entry.id, 0, 4096, Duration::ZERO)
        .await
        .unwrap();
    assert!(
        result["output"]["text"]
            .as_str()
            .unwrap()
            .contains("NATURAL_EXIT_OK")
    );
    assert!(entry.snapshot().get("tail_text").is_none());
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(runtime::read_ownership(&monitor.host).unwrap().is_empty());
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nonzero_result_fails_bgshell_and_spawn_failure_is_direct() {
    let monitor = Monitor::start().await;
    let manager = Manager::new().await.unwrap();
    let entry = manager
        .start(&monitor.host, command("exit 7"))
        .await
        .unwrap();
    assert_eq!(
        wait::run(manager.instance_id(), &entry.id, manager.port())
            .await
            .unwrap(),
        1
    );
    let spec = StartSpec::new(
        "pty-bridge-nonexistent-program",
        vec![],
        std::env::current_dir().unwrap(),
    );
    assert!(manager.start(&monitor.host, spec).await.is_err());
    assert_eq!(manager.snapshots(None).unwrap().len(), 1);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn monitor_disconnect_cleans_every_owned_pty_but_not_other_hosts() {
    let mut first = Monitor::start().await;
    let second = Monitor::start().await;
    let manager = Manager::new().await.unwrap();
    let a = manager.start(&first.host, long_running()).await.unwrap();
    let b = manager.start(&first.host, long_running()).await.unwrap();
    let c = manager.start(&second.host, long_running()).await.unwrap();
    let instance = manager.instance_id().to_string();
    let id = a.id.clone();
    let port = manager.port();
    let background = tokio::spawn(async move { wait::run(&instance, &id, port).await });
    first.kill();
    finished(&a).await;
    finished(&b).await;
    assert_eq!(
        a.snapshot()["termination"]["reason"],
        "monitor_disconnected"
    );
    assert_eq!(background.await.unwrap().unwrap(), 1);
    assert_eq!(c.session.snapshot().state, SessionState::Running);
    manager.close(&c.id).unwrap();
    finished(&c).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bgshell_disconnect_terminates_only_its_pty() {
    let monitor = Monitor::start().await;
    let manager = Manager::new().await.unwrap();
    let a = manager.start(&monitor.host, long_running()).await.unwrap();
    let b = manager.start(&monitor.host, long_running()).await.unwrap();
    let stream = wait_connection(&manager, &a.id).await;
    drop(stream);
    finished(&a).await;
    assert_eq!(
        a.snapshot()["termination"]["reason"],
        "bgshell_disconnected"
    );
    assert_eq!(b.session.snapshot().state, SessionState::Running);
    manager.close(&b.id).unwrap();
    finished(&b).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn duplicate_waiter_cannot_displace_the_original() {
    let monitor = Monitor::start().await;
    let manager = Manager::new().await.unwrap();
    let entry = manager.start(&monitor.host, long_running()).await.unwrap();
    let mut first = wait_connection(&manager, &entry.id).await;
    assert!(
        wait::run(manager.instance_id(), &entry.id, manager.port())
            .await
            .is_err()
    );
    manager.close(&entry.id).unwrap();
    let result: Value = receive(&mut first).await.unwrap();
    assert_eq!(result["termination"]["reason"], "explicit_close");
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn monitor_missing_never_creates_a_target() {
    let manager = Manager::new().await.unwrap();
    let host = format!("host_{}", uuid::Uuid::new_v4().simple());
    let error = manager.start(&host, long_running()).await.err().unwrap();
    assert!(error.to_string().contains("monitor not ready"));
    assert!(manager.snapshots(None).unwrap().is_empty());
    assert!(runtime::read_ownership(&host).unwrap().is_empty());
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hooks_inject_owner_acknowledge_read_and_clean_up() {
    let monitor = Monitor::start().await;
    let manager = Manager::new().await.unwrap();
    let input = json!({"program":"sh","args":["-c","echo x"],"env":{"EXAMPLE":"a"},"host_session_id":"incorrect"});
    let prepared = hook(
        "prepare",
        json!({"session_id":monitor.host,"tool_input":input}),
    );
    assert_eq!(
        prepared["hookSpecificOutput"]["updatedInput"]["host_session_id"],
        monitor.host
    );
    assert_eq!(
        prepared["hookSpecificOutput"]["updatedInput"]["args"],
        input["args"]
    );
    assert!(
        prepared["hookSpecificOutput"]
            .get("permissionDecision")
            .is_none()
    );
    let entry = manager.start(&monitor.host, long_running()).await.unwrap();
    let result = manager
        .read(&entry.id, 0, 4096, Duration::from_secs(1))
        .await
        .unwrap();
    hook(
        "observe",
        json!({"session_id":monitor.host,"tool_response":json!({"structuredContent":result}).to_string()}),
    );
    hook("cleanup", json!({"session_id":monitor.host}));
    finished(&entry).await;
    assert_eq!(
        entry.snapshot()["termination"]["reason"],
        "host_session_ended"
    );
    assert!(runtime::read_ownership(&monitor.host).unwrap().is_empty());
}
#[cfg(unix)]
#[test]
fn session_end_kills_process_tree_without_mcp_server() {
    use std::os::unix::process::CommandExt;
    let host = format!("host_{}", uuid::Uuid::new_v4().simple());
    let mut child = Command::new("/bin/sh")
        .args(["-c", "trap '' HUP TERM; sleep 30"])
        .process_group(0)
        .spawn()
        .unwrap();
    let pid = child.id() as i32;
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let record = runtime::Ownership {
        host_session_id: host.clone(),
        instance_id: "inst_fallback".into(),
        session_id: "pty_fallback".into(),
        port,
        process: pty_core::platform::ProcessLocator::Unix {
            process_id: pid,
            process_group: Some(pid),
        },
    };
    runtime::write_ownership(&record).unwrap();
    hook("cleanup", json!({"session_id":host}));
    assert!(!child.wait().unwrap().success());
    assert!(runtime::read_ownership(&host).unwrap().is_empty());
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_creation_respects_limit_and_shutdown_reaps_everything() {
    let monitor = Monitor::start().await;
    let manager = Manager::new().await.unwrap();
    let mut jobs = vec![];
    for _ in 0..pty_bridge::MAX_SESSIONS + 8 {
        let manager = Arc::clone(&manager);
        let host = monitor.host.clone();
        jobs.push(tokio::spawn(async move {
            manager.start(&host, long_running()).await
        }));
    }
    let mut entries = vec![];
    let mut failures = vec![];
    for job in jobs {
        match job.await.unwrap() {
            Ok(entry) => entries.push(entry),
            Err(error) => failures.push(format!("{error:#}")),
        }
    }
    assert_eq!(entries.len(), pty_bridge::MAX_SESSIONS, "{failures:?}");
    assert_eq!(
        manager.snapshots(None).unwrap().len(),
        pty_bridge::MAX_SESSIONS
    );
    manager.shutdown();
    for entry in entries {
        finished(&entry).await;
        assert_eq!(entry.snapshot()["termination"]["reason"], "server_shutdown");
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_racing_start_never_leaves_running_session() {
    let monitor = Monitor::start().await;
    let manager = Manager::new().await.unwrap();
    let start = {
        let manager = manager.clone();
        let host = monitor.host.clone();
        tokio::spawn(async move { manager.start(&host, long_running()).await })
    };
    tokio::task::yield_now().await;
    manager.shutdown();
    if let Ok(entry) = start.await.unwrap() {
        finished(&entry).await;
    }
    for value in manager.snapshots(None).unwrap() {
        assert_eq!(value["state"], "finished");
    }
}
