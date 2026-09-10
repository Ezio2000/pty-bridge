use anyhow::{Result, bail};
use serde::{Serialize, de::DeserializeOwned};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub async fn send(writer: &mut (impl AsyncWrite + Unpin), value: &impl Serialize) -> Result<()> {
    writer.write_all(&serde_json::to_vec(value)?).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await?;
    Ok(())
}

pub async fn receive<T: DeserializeOwned>(reader: &mut (impl AsyncBufRead + Unpin)) -> Result<T> {
    let mut line = String::new();
    reader.take(64 * 1024).read_line(&mut line).await?;
    if !line.ends_with('\n') {
        bail!("local connection closed or message incomplete");
    }
    Ok(serde_json::from_str(&line)?)
}
