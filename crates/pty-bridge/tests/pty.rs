use std::{
    io::Write,
    process::{Command, Stdio},
    sync::Arc,
    time::Duration,
};

use pty_bridge::{
    MAX_SESSIONS, background_task,
    manager::{FinishReason, Manager, SessionState, StartSpec, default_spec},
    runtime,
};

fn marker_spec(marker: &str) -> StartSpec {
    #[cfg(unix)]
    return default_spec(
        "/bin/sh".into(),
        vec!["-c".into(), format!("printf {marker}")],
        std::env::current_dir().unwrap(),
    );
    #[cfg(windows)]
    return default_spec(
        "cmd.exe".into(),
        vec!["/C".into(), format!("echo {marker}")],
        std::env::current_dir().unwrap(),
    );
}

fn long_running_spec() -> StartSpec {
    #[cfg(unix)]
    return default_spec(
        "/bin/sh".into(),
        vec!["-c".into(), "printf READY; sleep 30".into()],
        std::env::current_dir().unwrap(),
    );
    #[cfg(windows)]
    return default_spec(
        "cmd.exe".into(),
        vec!["/C".into(), "echo READY & ping -n 30 127.0.0.1 >NUL".into()],
        std::env::current_dir().unwrap(),
    );
}

async fn wait_for_output(manager: &Manager, session_id: &str, needle: &str) -> String {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
    loop {
        let output = manager.read(session_id, 0, 1024 * 1024).unwrap();
        let text = String::from_utf8_lossy(&output.bytes).into_owned();
        if text.contains(needle) {
            return text;
        }
        assert!(tokio::time::Instant::now() < deadline, "output: {text:?}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn wait_for_state(manager: &Manager, session_id: &str, state: SessionState) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
    while manager.state(session_id).unwrap() != state {
        assert!(tokio::time::Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interactive_session_is_a_real_tty() {
    let manager = Manager::new().await.unwrap();
    let spec = default_spec("/bin/sh".into(), vec![], std::env::current_dir().unwrap());
    let (session_id, _) = manager.create(spec, Duration::from_secs(30)).unwrap();

    let instance_id = manager.instance_id().to_string();
    let attached_id = session_id.clone();
    let background =
        tokio::spawn(async move { background_task::run(&instance_id, &attached_id).await });

    wait_for_state(&manager, &session_id, SessionState::Running).await;
    manager
        .write(
            &session_id,
            "test -t 0 && test -t 1 && test -t 2 && echo PTY_OK\n",
        )
        .unwrap();
    let output = wait_for_output(&manager, &session_id, "PTY_OK").await;
    assert!(output.contains("PTY_OK"));
    manager.close(&session_id).unwrap();
    background.await.unwrap().unwrap();
    let snapshot = manager.snapshots(Some(&session_id)).unwrap().remove(0);
    assert_eq!(snapshot.state, SessionState::Finished);
    assert_eq!(
        snapshot.termination.unwrap().reason,
        FinishReason::ExplicitClose
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn natural_exit_is_fully_finalized() {
    let manager = Manager::new().await.unwrap();
    let (session_id, launch) = manager
        .create(marker_spec("NATURAL_EXIT_OK"), Duration::from_secs(30))
        .unwrap();
    assert!(launch.command.contains("background-task"));
    assert_eq!(
        manager.state(&session_id).unwrap(),
        SessionState::AwaitingBackgroundTask
    );
    let instance_id = manager.instance_id().to_string();
    let host_session = format!("host_{}", uuid::Uuid::new_v4().simple());
    run_hook(
        "bind",
        serde_json::json!({
            "session_id": host_session,
            "tool_response": serde_json::json!({
                "instance_id": manager.instance_id(),
                "session_id": session_id,
                "control_port": manager.port()
            })
        }),
    );
    let control_path = runtime::runtime_root_path()
        .unwrap()
        .join(&instance_id)
        .join(format!("{session_id}.control"));
    let ticket_path = runtime::background_task_ticket_path(&instance_id, &session_id).unwrap();
    assert!(control_path.is_file());
    assert!(ticket_path.is_file());

    let attached_id = session_id.clone();
    let background =
        tokio::spawn(async move { background_task::run(&instance_id, &attached_id).await });
    let output = wait_for_output(&manager, &session_id, "NATURAL_EXIT_OK").await;
    assert!(output.contains("NATURAL_EXIT_OK"));
    background.await.unwrap().unwrap();
    wait_for_state(&manager, &session_id, SessionState::Finished).await;

    let snapshot = manager.snapshots(Some(&session_id)).unwrap().remove(0);
    let termination = snapshot.termination.unwrap();
    assert_eq!(termination.reason, FinishReason::NaturalExit);
    assert_eq!(termination.exit_code, Some(0));
    assert!(!control_path.exists());
    assert!(!ticket_path.exists());
    let scan = runtime::read_ownership(&host_session).unwrap();
    assert!(scan.entries.is_empty());
    assert!(scan.errors.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn background_task_disconnect_finishes_process_tree() {
    let manager = Manager::new().await.unwrap();
    let (session_id, _) = manager
        .create(long_running_spec(), Duration::from_secs(30))
        .unwrap();
    let instance_id = manager.instance_id().to_string();
    let attached_id = session_id.clone();
    let background =
        tokio::spawn(async move { background_task::run(&instance_id, &attached_id).await });
    wait_for_output(&manager, &session_id, "READY").await;

    background.abort();
    assert!(background.await.unwrap_err().is_cancelled());
    wait_for_state(&manager, &session_id, SessionState::Finished).await;
    let snapshot = manager.snapshots(Some(&session_id)).unwrap().remove(0);
    assert_eq!(
        snapshot.termination.unwrap().reason,
        FinishReason::BackgroundTaskDisconnected
    );
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disconnect_force_kills_a_signal_ignoring_process() {
    let manager = Manager::new().await.unwrap();
    let spec = default_spec(
        "/bin/sh".into(),
        vec![
            "-c".into(),
            "trap '' HUP TERM; printf 'PID=%s\\n' $$; sleep 30".into(),
        ],
        std::env::current_dir().unwrap(),
    );
    let (session_id, _) = manager.create(spec, Duration::from_secs(30)).unwrap();
    let instance_id = manager.instance_id().to_string();
    let attached_id = session_id.clone();
    let background =
        tokio::spawn(async move { background_task::run(&instance_id, &attached_id).await });
    let output = wait_for_output(&manager, &session_id, "PID=").await;
    let pid = output
        .split("PID=")
        .nth(1)
        .unwrap()
        .lines()
        .next()
        .unwrap()
        .trim()
        .parse::<i32>()
        .unwrap();

    background.abort();
    let _ = background.await;
    wait_for_state(&manager, &session_id, SessionState::Finished).await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        let alive = unsafe { libc::kill(pid, 0) } == 0;
        if !alive {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "process {pid} survived"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn run_hook(command: &str, input: serde_json::Value) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_pty-bridge"))
        .args(["hook", command])
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(serde_json::to_string(&input).unwrap().as_bytes())
        .unwrap();
    assert!(child.wait().unwrap().success());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn session_end_hook_finishes_owned_session() {
    let manager = Manager::new().await.unwrap();
    let (session_id, _) = manager
        .create(long_running_spec(), Duration::from_secs(30))
        .unwrap();
    let host_session = format!("host_{}", uuid::Uuid::new_v4().simple());
    run_hook(
        "bind",
        serde_json::json!({
            "session_id": host_session,
            "tool_response": serde_json::json!({
                "instance_id": manager.instance_id(),
                "session_id": session_id,
                "control_port": manager.port()
            }).to_string()
        }),
    );

    let instance_id = manager.instance_id().to_string();
    let attached_id = session_id.clone();
    let background =
        tokio::spawn(async move { background_task::run(&instance_id, &attached_id).await });
    wait_for_state(&manager, &session_id, SessionState::Running).await;
    run_hook("cleanup", serde_json::json!({ "session_id": host_session }));

    wait_for_state(&manager, &session_id, SessionState::Finished).await;
    let snapshot = manager.snapshots(Some(&session_id)).unwrap().remove(0);
    assert_eq!(
        snapshot.termination.unwrap().reason,
        FinishReason::HostSessionEnded
    );
    background.await.unwrap().unwrap();
    let scan = runtime::read_ownership(&host_session).unwrap();
    assert!(scan.entries.is_empty());
    assert!(scan.errors.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn close_during_start_cannot_resurrect_session() {
    let manager = Manager::new().await.unwrap();
    let mut spec = long_running_spec();
    for index in 0..10_000 {
        spec.env.insert(format!("PAD_{index}"), "x".repeat(64));
    }
    let (session_id, _) = manager.create(spec, Duration::from_secs(30)).unwrap();
    let instance_id = manager.instance_id().to_string();
    let attached_id = session_id.clone();
    let background =
        tokio::spawn(async move { background_task::run(&instance_id, &attached_id).await });
    tokio::task::yield_now().await;
    manager.close(&session_id).unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;

    let snapshot = manager.snapshots(Some(&session_id)).unwrap().remove(0);
    assert_eq!(snapshot.state, SessionState::Finished);
    assert_eq!(
        snapshot.termination.unwrap().reason,
        FinishReason::ExplicitClose
    );
    let _ = background.await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_create_never_exceeds_session_limit() {
    let manager = Manager::new().await.unwrap();
    let mut tasks = Vec::new();
    for _ in 0..(MAX_SESSIONS * 2) {
        let manager = Arc::clone(&manager);
        tasks.push(tokio::spawn(async move {
            manager.create(marker_spec("LIMIT"), Duration::from_secs(30))
        }));
    }
    let mut created = 0;
    for task in tasks {
        if task.await.unwrap().is_ok() {
            created += 1;
        }
    }
    assert_eq!(created, MAX_SESSIONS);
    assert_eq!(manager.snapshots(None).unwrap().len(), MAX_SESSIONS);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn start_failure_is_reported_and_finalized() {
    let manager = Manager::new().await.unwrap();
    let spec = default_spec(
        "pty-bridge-command-that-does-not-exist".into(),
        vec![],
        std::env::current_dir().unwrap(),
    );
    let (session_id, _) = manager.create(spec, Duration::from_secs(30)).unwrap();
    let instance_id = manager.instance_id().to_string();
    let attached_id = session_id.clone();
    let result = background_task::run(&instance_id, &attached_id).await;
    assert!(result.is_err());
    wait_for_state(&manager, &session_id, SessionState::Finished).await;
    let snapshot = manager.snapshots(Some(&session_id)).unwrap().remove(0);
    assert_eq!(
        snapshot.termination.unwrap().reason,
        FinishReason::StartFailed
    );
}
