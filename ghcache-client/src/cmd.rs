use anyhow::Result;
use std::path::PathBuf;

pub const DEFAULT_PORT: u16 = 7748;

async fn post_json(port: u16, path: &str, body: serde_json::Value) -> Result<serde_json::Value> {
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://127.0.0.1:{port}{path}"))
        .json(&body)
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("connect to ghcache cmd server on port {port}: {e}"))?;
    Ok(resp.json().await?)
}

/// Subscribe to a repo and return its local clone path.
///
/// Clones `owner/repo` under `staging_folder` if not present, fetches latest,
/// and registers `uuid` as an active subscriber. If `pr_sync` is true, ghcache
/// will also run the full PR/branch sync pipeline for this repo while the
/// subscription is alive.
///
/// Call [`heartbeat`] periodically to keep the subscription alive.
pub async fn ensure_repo(
    port: u16,
    uuid: &str,
    owner: &str,
    repo: &str,
    pr_sync: bool,
    notifications: bool,
) -> Result<PathBuf> {
    let resp = post_json(port, "/subscribe", serde_json::json!({
        "uuid":          uuid,
        "owner":         owner,
        "repo":          repo,
        "pr_sync":       pr_sync,
        "notifications": notifications,
    }))
    .await?;
    let path = resp["path"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing 'path' in subscribe response"))?;
    Ok(PathBuf::from(path))
}

/// Renew a subscriber's TTL. Returns false if the UUID is unknown (subscription expired).
pub async fn heartbeat(port: u16, uuid: &str) -> Result<bool> {
    let resp = post_json(port, "/heartbeat", serde_json::json!({"uuid": uuid})).await?;
    Ok(resp["ok"].as_bool().unwrap_or(false))
}

/// Pause the ghcache sync loop.
pub async fn pause(port: u16) -> Result<()> {
    post_json(port, "/pause", serde_json::json!({})).await?;
    Ok(())
}

/// Resume the ghcache sync loop after a pause.
pub async fn resume(port: u16) -> Result<()> {
    post_json(port, "/resume", serde_json::json!({})).await?;
    Ok(())
}
