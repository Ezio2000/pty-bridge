use pty_core::{FinishReason, Session, SessionState, StartSpec};
use std::time::Duration;

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
async fn finished(s: &Session) -> pty_core::Termination {
    tokio::time::timeout(Duration::from_secs(6), s.wait())
        .await
        .expect("PTY did not finish")
}
async fn output(s: &Session, needle: &str) -> String {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let bytes = s.reader().read(0, 1024 * 1024).bytes;
        let text = String::from_utf8_lossy(&bytes).into_owned();
        if text.contains(needle) {
            return text;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "missing {needle}: {text:?}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}
#[tokio::test]
async fn immediate_exit_retains_output_and_is_replayable() {
    #[cfg(unix)]
    let spec = command("printf CORE_FINAL");
    #[cfg(windows)]
    let spec = command("echo CORE_FINAL");
    let s = Session::start(spec).unwrap();
    let t = finished(&s).await;
    assert_eq!(t.reason, FinishReason::NaturalExit);
    assert_eq!(t.exit_code, Some(0));
    assert!(String::from_utf8_lossy(&s.reader().read(0, 1000).bytes).contains("CORE_FINAL"));
    assert_eq!(finished(&s).await.exit_code, Some(0));
    s.close().unwrap();
    assert_eq!(
        s.snapshot().termination.unwrap().reason,
        FinishReason::NaturalExit
    );
}
#[tokio::test]
async fn nonzero_exit_and_start_failure_are_distinct() {
    let s = Session::start(command("exit 7")).unwrap();
    let t = finished(&s).await;
    assert_eq!(t.reason, FinishReason::NaturalExit);
    assert_eq!(t.exit_code, Some(7));
    assert!(
        Session::start(StartSpec::new(
            "pty-core-nonexistent-program",
            vec![],
            std::env::current_dir().unwrap()
        ))
        .is_err()
    );
}
#[cfg(unix)]
#[tokio::test]
async fn real_tty_separate_readers_resize_and_input_receipt() {
    let s=Session::start(command("test -t 0 && test -t 1 && test -t 2 && printf TTY_READY; read line; printf 'GOT:%s' \"$line\"")).unwrap();
    output(&s, "TTY_READY").await;
    s.resize(30, 100).unwrap();
    assert_eq!(s.snapshot().rows, 30);
    let input = s.writer().write(b"hello\n").await.unwrap();
    assert_eq!(input.bytes_written, 6);
    assert_eq!(input.interaction_id, 2);
    finished(&s).await;
    let all = s.reader().read(0, 4096);
    assert!(String::from_utf8_lossy(&all.bytes).contains("GOT:hello"));
    assert_eq!(s.reader().read(0, 3).bytes, all.bytes[..3]);
    assert_eq!(s.reader().read(3, 4096).bytes, all.bytes[3..]);
}
#[cfg(windows)]
#[tokio::test]
async fn conpty_input_resize_and_output_drain() {
    let s = Session::start(command(
        "echo TTY_READY & set /p answer= & echo INPUT_RECEIVED",
    ))
    .unwrap();
    output(&s, "TTY_READY").await;
    s.resize(30, 100).unwrap();
    assert_eq!(
        s.writer().write(b"hello\r\n").await.unwrap().bytes_written,
        7
    );
    finished(&s).await;
    output(&s, "INPUT_RECEIVED").await;
}
#[cfg(unix)]
#[tokio::test]
async fn terminal_queries_use_writer_and_do_not_create_user_interactions() {
    let s = Session::start(command(
        "stty raw -echo; printf '\\033[5'; printf 'n'; dd bs=1 count=4 2>/dev/null | od -An -tx1",
    ))
    .unwrap();
    let t = finished(&s).await;
    assert_eq!(t.reason, FinishReason::NaturalExit);
    let text = String::from_utf8_lossy(&s.reader().read(0, 4096).bytes).into_owned();
    assert_eq!(
        text.split_whitespace().collect::<Vec<_>>(),
        ["1b", "5b", "30", "6e"]
    );
    assert_eq!(s.snapshot().activity.interaction_id, 1);
}
#[cfg(unix)]
#[tokio::test]
async fn cursor_and_attribute_queries_answer_from_the_screen_in_order() {
    let s = Session::start(command(
        "stty raw -echo; printf '\\n\\nabc\\033[6n\\033[c'; dd bs=1 count=13 2>/dev/null | od -An -tx1",
    ))
    .unwrap();
    assert_eq!(finished(&s).await.reason, FinishReason::NaturalExit);
    let text = String::from_utf8_lossy(&s.reader().read(0, 4096).bytes).into_owned();
    let hex: Vec<_> = text.split_whitespace().skip(1).collect();
    // ESC [ 3 ; 4 R, then ESC [ ? 1 ; 2 c
    assert_eq!(
        hex,
        [
            "1b", "5b", "33", "3b", "34", "52", "1b", "5b", "3f", "31", "3b", "32", "63"
        ]
    );
}
#[cfg(unix)]
#[tokio::test]
async fn arrow_keys_follow_the_application_cursor_mode() {
    let s = Session::start(command(
        "stty raw -echo; printf '\\033[?1hKEYS_READY'; dd bs=1 count=9 2>/dev/null | od -An -tx1",
    ))
    .unwrap();
    output(&s, "KEYS_READY").await;
    assert!(s.application_cursor());
    let keys = pty_core::keys::encode_all(&["Up".into(), "C-Left".into()], s.application_cursor());
    s.writer().write(&keys.unwrap()).await.unwrap();
    // Up in cursor-key mode is SS3; modified keys keep the CSI form.
    assert_eq!(finished(&s).await.reason, FinishReason::NaturalExit);
    let text = String::from_utf8_lossy(&s.reader().read(0, 4096).bytes).into_owned();
    let hex: Vec<_> = text
        .rsplit("KEYS_READY")
        .next()
        .unwrap()
        .split_whitespace()
        .collect();
    assert_eq!(hex, ["1b", "4f", "41", "1b", "5b", "31", "3b", "35", "44"]);
}
#[cfg(unix)]
#[tokio::test]
async fn blocked_write_times_out_and_does_not_hold_lifecycle_lock() {
    let s = Session::start(command("stty raw -echo; printf BLOCK_READY; sleep 30")).unwrap();
    output(&s, "BLOCK_READY").await;
    let error = s
        .writer()
        .write_with_timeout(&vec![b'x'; 1024 * 1024], Duration::from_millis(150))
        .await
        .unwrap_err();
    assert!(error.bytes_written < 1024 * 1024);
    assert!(error.delivery_uncertain || error.message.contains("ending"));
    assert_eq!(finished(&s).await.reason, FinishReason::WriteTimeout);
}
#[cfg(unix)]
#[tokio::test]
async fn close_cancels_blocked_writer_and_preserves_termination_reason() {
    let s = Session::start(command("stty raw -echo; printf BLOCK_READY; sleep 30")).unwrap();
    output(&s, "BLOCK_READY").await;
    let writer = s.writer();
    let task = tokio::spawn(async move { writer.write(&vec![b'x'; 1024 * 1024]).await });
    tokio::time::sleep(Duration::from_millis(50)).await;
    s.close().unwrap();
    s.close().unwrap();
    assert_eq!(finished(&s).await.reason, FinishReason::ExplicitClose);
    assert!(
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
}
#[cfg(unix)]
#[tokio::test]
async fn dropping_last_public_handle_stops_process_tree() {
    let s = Session::start(command("trap '' HUP TERM; printf OWNER_READY; sleep 30")).unwrap();
    output(&s, "OWNER_READY").await;
    let pty_core::platform::ProcessLocator::Unix { process_id, .. } = s.process_locator() else {
        panic!()
    };
    drop(s);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while unsafe { libc::kill(process_id, 0) } == 0 {
        assert!(tokio::time::Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}
#[tokio::test]
async fn empty_write_and_metadata_do_not_rearm_activity() {
    #[cfg(unix)]
    let spec = command("sleep 30");
    #[cfg(windows)]
    let spec = command("ping -n 30 127.0.0.1 >NUL");
    let s = Session::start(spec).unwrap();
    let before = s.snapshot().activity;
    s.writer().write(b"").await.unwrap();
    s.reader().read(0, 100);
    s.resize(40, 90).unwrap();
    assert_eq!(s.snapshot().activity.interaction_id, before.interaction_id);
    s.close().unwrap();
    finished(&s).await;
    assert_eq!(s.snapshot().state, SessionState::Finished);
}
