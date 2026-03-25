use anyhow::Result;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;

pub const DEFAULT_PORT: u16 = 7748;

fn post_json(port: u16, path: &str, body: &str) -> Result<serde_json::Value> {
    let mut stream = TcpStream::connect(format!("127.0.0.1:{port}"))
        .map_err(|e| anyhow::anyhow!("connect to ghcache cmd server on port {port}: {e}"))?;

    let req = format!(
        "POST {} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        path,
        body.len(),
        body
    );
    stream.write_all(req.as_bytes())?;

    let mut reader = BufReader::new(&stream);

    // Discard status line
    let mut status_line = String::new();
    reader.read_line(&mut status_line)?;

    let mut content_length: usize = 0;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line)?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            break;
        }
        if trimmed.to_ascii_lowercase().starts_with("content-length:") {
            content_length = trimmed[15..].trim().parse().unwrap_or(0);
        }
    }

    let mut resp_body = vec![0u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut resp_body)?;
    }

    Ok(serde_json::from_slice(&resp_body)?)
}

/// Subscribe to a repo and return its local clone path.
///
/// Clones `owner/repo` under `staging_folder` if not present, fetches latest,
/// and registers `uuid` as an active subscriber. If `pr_sync` is true, ghcacher
/// will also run the full PR/branch sync pipeline for this repo while the
/// subscription is alive.
///
/// Call [`heartbeat`] periodically to keep the subscription alive.
pub fn ensure_repo(port: u16, uuid: &str, owner: &str, repo: &str, pr_sync: bool, notifications: bool) -> Result<PathBuf> {
    let body = serde_json::json!({
        "uuid": uuid,
        "owner": owner,
        "repo": repo,
        "pr_sync": pr_sync,
        "notifications": notifications,
    })
    .to_string();

    let resp = post_json(port, "/subscribe", &body)?;
    let path = resp["path"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing 'path' in subscribe response"))?;
    Ok(PathBuf::from(path))
}

/// Renew a subscriber's TTL. Returns false if the UUID is unknown (subscription expired).
pub fn heartbeat(port: u16, uuid: &str) -> Result<bool> {
    let body = serde_json::json!({"uuid": uuid}).to_string();
    let resp = post_json(port, "/heartbeat", &body)?;
    Ok(resp["ok"].as_bool().unwrap_or(false))
}

/// Pause the ghcache sync loop.
pub fn pause(port: u16) -> Result<()> {
    post_json(port, "/pause", "{}")?;
    Ok(())
}

/// Resume the ghcache sync loop after a pause.
pub fn resume(port: u16) -> Result<()> {
    post_json(port, "/resume", "{}")?;
    Ok(())
}
