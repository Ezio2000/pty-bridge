use crate::{
    protocol::{receive, send},
    runtime::{self, MonitorRecord},
};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::{fs, io::Write, time::Duration};
use tokio::{
    io::BufReader,
    net::{TcpListener, TcpStream},
    sync::mpsc,
};

/// One Claude-owned process; stdout contains only actual stall notices.
pub async fn run(host: &str) -> Result<()> {
    runtime::validate_id(host)?;
    let path = runtime::monitor_path(host)?;
    if let Ok(existing) = runtime::read_monitor(host) {
        if tokio::time::timeout(
            Duration::from_millis(250),
            TcpStream::connect(("127.0.0.1", existing.port)),
        )
        .await
        .is_ok_and(|r| r.is_ok())
        {
            bail!("a monitor for this Claude session is already running");
        }
        let _ = fs::remove_file(&path);
    }
    fs::create_dir_all(path.parent().unwrap())?;
    let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
    let record = MonitorRecord {
        host_session_id: host.into(),
        generation: uuid::Uuid::new_v4().simple().to_string(),
        port: listener.local_addr()?.port(),
    };
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)?;
    serde_json::to_writer(&mut file, &record)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    struct RegistryGuard(std::path::PathBuf);
    impl Drop for RegistryGuard {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }
    let _registry = RegistryGuard(path);
    let (notices, mut output) = mpsc::channel::<String>(64);
    loop {
        tokio::select! {
            accepted=listener.accept()=> {
                let (socket,_)=accepted?; let record=record.clone(); let notices=notices.clone();
                tokio::spawn(async move { if let Err(e)=serve(socket,record,notices).await { tracing::debug!("monitor connection: {e:#}"); } });
            }
            Some(line)=output.recv()=> {
                let mut stdout=std::io::stdout().lock(); writeln!(stdout,"{line}")?; stdout.flush()?;
            }
            _=tokio::signal::ctrl_c()=>return Ok(()),
        }
    }
}

async fn serve(
    socket: TcpStream,
    record: MonitorRecord,
    notices: mpsc::Sender<String>,
) -> Result<()> {
    let mut stream = BufReader::new(socket);
    let hello: Value = tokio::time::timeout(Duration::from_secs(5), receive(&mut stream)).await??;
    if hello["action"] != "register"
        || hello["host_session_id"] != record.host_session_id
        || hello["generation"] != record.generation
    {
        bail!("monitor identity mismatch");
    }
    send(
        stream.get_mut(),
        &json!({"ok":true,"generation":record.generation}),
    )
    .await?;
    loop {
        let request: Value = receive(&mut stream).await?;
        match request["action"].as_str() {
            Some("ping") => send(stream.get_mut(), &json!({"ok":true})).await?,
            Some("offer") => {
                // Reserve stdout capacity before asking the core owner to revalidate.
                let permit = notices.reserve().await.context("monitor stdout closed")?;
                send(stream.get_mut(),&json!({"action":"claim","session_id":request["session_id"],"candidate":request["candidate"]})).await?;
                let result: Value = receive(&mut stream).await?;
                if result["notify"] == true {
                    let session = request["session_id"]
                        .as_str()
                        .context("missing session id")?;
                    permit.send(format!("PTY {session} 疑似停滞，请调用 read 检查。"));
                }
                send(stream.get_mut(), &json!({"ok":true})).await?;
            }
            _ => bail!("unknown monitor message"),
        }
    }
}
