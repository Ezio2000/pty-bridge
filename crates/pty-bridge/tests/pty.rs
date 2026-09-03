#![cfg(unix)]

use std::{
    io::Write,
    process::{Command, Stdio},
    time::Duration,
};

use pty_bridge::{
    background_task,
    manager::{Manager, SessionState, default_spec},
};

async fn wait_for_output(manager: &Manager, session_id: &str, needle: &str) -> String {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interactive_session_is_a_real_tty() {
    let manager = Manager::new().await.unwrap();
    let spec = default_spec("/bin/sh".into(), vec![], std::env::current_dir().unwrap());
    let (session_id, _) = manager.create(spec, Duration::from_secs(30)).unwrap();

    let instance_id = manager.instance_id().to_string();
    let attached_id = session_id.clone();
    let background =
        tokio::spawn(async move { background_task::run(&instance_id, &attached_id).await });

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while manager.state(&session_id).unwrap() != SessionState::Running {
        assert!(tokio::time::Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
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
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_starts_after_background_task_attachment() {
    let manager = Manager::new().await.unwrap();
    let spec = default_spec(
        "/bin/sh".into(),
        vec!["-c".into(), "printf BACKGROUND_TASK_OK".into()],
        std::env::current_dir().unwrap(),
    );
    let (session_id, command) = manager.create(spec, Duration::from_secs(30)).unwrap();
    assert!(command.contains("background-task"));
    assert_eq!(
        manager.state(&session_id).unwrap(),
        SessionState::AwaitingBackgroundTask
    );

    let instance_id = manager.instance_id().to_string();
    let attached_id = session_id.clone();
    let background =
        tokio::spawn(async move { background_task::run(&instance_id, &attached_id).await });
    let output = wait_for_output(&manager, &session_id, "BACKGROUND_TASK_OK").await;
    assert!(output.contains("BACKGROUND_TASK_OK"));
    background.await.unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn background_task_disconnect_closes_session() {
    let manager = Manager::new().await.unwrap();
    let spec = default_spec(
        "/bin/sh".into(),
        vec!["-c".into(), "printf READY; sleep 30".into()],
        std::env::current_dir().unwrap(),
    );
    let (session_id, _) = manager.create(spec, Duration::from_secs(30)).unwrap();
    let instance_id = manager.instance_id().to_string();
    let attached_id = session_id.clone();
    let background =
        tokio::spawn(async move { background_task::run(&instance_id, &attached_id).await });
    wait_for_output(&manager, &session_id, "READY").await;

    background.abort();
    assert!(background.await.unwrap_err().is_cancelled());

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while manager.state(&session_id).unwrap() != SessionState::Closed {
        assert!(tokio::time::Instant::now() < deadline);
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_end_hook_closes_owned_session() {
    let manager = Manager::new().await.unwrap();
    let spec = default_spec("/bin/sh".into(), vec![], std::env::current_dir().unwrap());
    let (session_id, _) = manager.create(spec, Duration::from_secs(30)).unwrap();
    let instance_id = manager.instance_id().to_string();
    let attached_id = session_id.clone();
    let background =
        tokio::spawn(async move { background_task::run(&instance_id, &attached_id).await });
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while manager.state(&session_id).unwrap() != SessionState::Running {
        assert!(tokio::time::Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

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
    run_hook("cleanup", serde_json::json!({ "session_id": host_session }));

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while manager.state(&session_id).unwrap() != SessionState::Closed {
        assert!(tokio::time::Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    background.await.unwrap().unwrap();
}
