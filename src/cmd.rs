use anyhow::Result;
use rusqlite::Connection;
use serde::Deserialize;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

pub const DEFAULT_PORT: u16 = 7748;
pub const DEFAULT_TTL_SECS: u64 = 30;

// ── Subscription state ────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct SubscribeReq {
    uuid: String,
    owner: String,
    repo: String,
    #[serde(default)]
    pr_sync: bool,
    #[serde(default)]
    notifications: bool,
}

#[derive(Deserialize)]
struct HeartbeatReq {
    uuid: String,
}

#[derive(Clone)]
struct RepoSub {
    owner: String,
    repo: String,
    pr_sync: bool,
    notifications: bool,
}

struct Sub {
    last_seen: Instant,
    repos: Vec<RepoSub>,
}

pub struct Subscriptions {
    inner: RwLock<HashMap<String, Sub>>,
    ttl: Duration,
}

impl Subscriptions {
    pub fn new(ttl: Duration) -> Arc<Self> {
        Arc::new(Self {
            inner: RwLock::new(HashMap::new()),
            ttl,
        })
    }

    fn upsert(&self, uuid: String, owner: String, repo: String, pr_sync: bool, notifications: bool) {
        let mut g = self.inner.write().unwrap();
        let sub = g.entry(uuid).or_insert_with(|| Sub {
            last_seen: Instant::now(),
            repos: vec![],
        });
        sub.last_seen = Instant::now();
        match sub.repos.iter_mut().find(|r| r.owner == owner && r.repo == repo) {
            Some(r) => {
                r.pr_sync = r.pr_sync || pr_sync;
                r.notifications = r.notifications || notifications;
            }
            None => sub.repos.push(RepoSub { owner, repo, pr_sync, notifications }),
        }
    }

    fn heartbeat(&self, uuid: &str) -> bool {
        let mut g = self.inner.write().unwrap();
        match g.get_mut(uuid) {
            Some(sub) => {
                sub.last_seen = Instant::now();
                true
            }
            None => false,
        }
    }

    fn sweep(&self) {
        let ttl = self.ttl;
        self.inner
            .write()
            .unwrap()
            .retain(|_, sub| sub.last_seen.elapsed() < ttl);
    }

    /// All (owner, repo) pairs with at least one live subscriber flagged pr_sync.
    pub fn active_pr_sync_repos(&self) -> Vec<(String, String)> {
        self.active_repos_by(|r| r.pr_sync)
    }

    /// All (owner, repo) pairs with at least one live subscriber flagged notifications.
    pub fn active_notifications_repos(&self) -> Vec<(String, String)> {
        self.active_repos_by(|r| r.notifications)
    }

    fn active_repos_by(&self, pred: impl Fn(&RepoSub) -> bool) -> Vec<(String, String)> {
        let ttl = self.ttl;
        let g = self.inner.read().unwrap();
        let mut set = std::collections::HashSet::new();
        for sub in g.values() {
            if sub.last_seen.elapsed() < ttl {
                for r in &sub.repos {
                    if pred(r) {
                        set.insert((r.owner.clone(), r.repo.clone()));
                    }
                }
            }
        }
        set.into_iter().collect()
    }
}

// ── SSE client list ───────────────────────────────────────────────────────────

struct SseClient {
    stream: TcpStream,
    last_sent_id: i64,
}

type Clients = Arc<Mutex<Vec<SseClient>>>;

/// Fetch change_log rows with id > min_id. Returns (id, json_string) pairs.
fn fetch_changes(conn: &Connection, min_id: i64) -> Result<Vec<(i64, String)>> {
    let mut stmt = conn.prepare_cached(
        "SELECT id, entity_type, entity_id, event, repo_slug, payload_json, occurred_at
         FROM change_log WHERE id > ?1 ORDER BY id",
    )?;
    let mut rows = stmt.query(rusqlite::params![min_id])?;
    let mut out = vec![];
    while let Some(row) = rows.next()? {
        let id: i64 = row.get(0)?;
        let entity_type: String = row.get(1)?;
        let entity_id: i64 = row.get(2)?;
        let event: String = row.get(3)?;
        let repo_slug: Option<String> = row.get(4)?;
        let payload_json: Option<String> = row.get(5)?;
        let occurred_at: String = row.get(6)?;

        let payload: serde_json::Value = payload_json
            .as_deref()
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or(serde_json::Value::Null);

        let json = serde_json::to_string(&serde_json::json!({
            "id": id,
            "entity_type": entity_type,
            "entity_id": entity_id,
            "event": event,
            "repo_slug": repo_slug,
            "payload": payload,
            "occurred_at": occurred_at,
        }))?;
        out.push((id, json));
    }
    Ok(out)
}

fn broadcast_loop(clients: Clients, db_path: PathBuf) {
    let conn = match Connection::open_with_flags(
        &db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "SSE broadcast: failed to open DB");
            return;
        }
    };
    let _ = conn.execute_batch("PRAGMA journal_mode = WAL;");

    loop {
        std::thread::sleep(Duration::from_millis(500));

        let mut guard = clients.lock().unwrap();
        if guard.is_empty() {
            continue;
        }

        let min_id = guard.iter().map(|c| c.last_sent_id).min().unwrap_or(0);
        let rows = match fetch_changes(&conn, min_id) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, "SSE broadcast: DB poll failed");
                continue;
            }
        };
        if rows.is_empty() {
            continue;
        }

        guard.retain_mut(|client| {
            let threshold = client.last_sent_id;
            for (id, json) in rows.iter().filter(|(id, _)| *id > threshold) {
                let frame = format!("id: {id}\ndata: {json}\n\n");
                if client.stream.write_all(frame.as_bytes()).is_err() {
                    return false;
                }
                client.last_sent_id = *id;
            }
            true
        });
    }
}

// ── HTTP server ───────────────────────────────────────────────────────────────

pub fn run(
    subs: Arc<Subscriptions>,
    staging: PathBuf,
    db_path: PathBuf,
    port: u16,
    paused: Arc<AtomicBool>,
) -> Result<()> {
    let listener = TcpListener::bind(format!("127.0.0.1:{port}"))
        .map_err(|e| anyhow::anyhow!("cmd HTTP bind 127.0.0.1:{port}: {e}"))?;
    tracing::info!(port, "cmd+SSE server listening on 127.0.0.1");

    let clients: Clients = Arc::new(Mutex::new(vec![]));

    // Broadcast loop thread
    let clients_bcast = Arc::clone(&clients);
    std::thread::spawn(move || broadcast_loop(clients_bcast, db_path));

    // Sweep thread
    let subs_sweep = Arc::clone(&subs);
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_secs(10));
        subs_sweep.sweep();
    });

    for stream in listener.incoming() {
        match stream {
            Ok(s) => {
                let subs = Arc::clone(&subs);
                let clients = Arc::clone(&clients);
                let staging = staging.clone();
                let paused = Arc::clone(&paused);
                std::thread::spawn(move || {
                    if let Err(e) = handle(s, &subs, &staging, clients, paused) {
                        tracing::warn!(error = %e, "cmd: request failed");
                    }
                });
            }
            Err(e) => tracing::warn!(error = %e, "cmd: accept error"),
        }
    }
    Ok(())
}

struct RequestHead {
    method: String,
    path: String,
    content_length: usize,
    last_event_id: i64,
}

fn parse_head(reader: &mut BufReader<&TcpStream>) -> Result<Option<RequestHead>> {
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    let parts: Vec<&str> = request_line.trim().splitn(3, ' ').collect();
    if parts.len() < 2 {
        return Ok(None);
    }
    let method = parts[0].to_string();
    let path = parts[1].to_string();

    let mut content_length: usize = 0;
    let mut last_event_id: i64 = 0;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line)?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            break;
        }
        let lower = trimmed.to_ascii_lowercase();
        if lower.starts_with("content-length:") {
            content_length = trimmed[15..].trim().parse().unwrap_or(0);
        } else if lower.starts_with("last-event-id:") {
            last_event_id = trimmed[14..].trim().parse().unwrap_or(0);
        }
    }

    Ok(Some(RequestHead { method, path, content_length, last_event_id }))
}

fn handle(mut stream: TcpStream, subs: &Subscriptions, staging: &Path, clients: Clients, paused: Arc<AtomicBool>) -> Result<()> {
    let mut reader = BufReader::new(&stream);
    let head = match parse_head(&mut reader)? {
        Some(h) => h,
        None => return Ok(()),
    };

    let mut body = vec![0u8; head.content_length];
    if head.content_length > 0 {
        reader.read_exact(&mut body)?;
    }

    match (head.method.as_str(), head.path.as_str()) {
        ("GET", "/events") => {
            stream.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: keep-alive\r\n\r\n",
            )?;
            clients.lock().unwrap().push(SseClient {
                stream,
                last_sent_id: head.last_event_id,
            });
            // Connection is now owned by the broadcast loop; don't close it.
            Ok(())
        }
        ("POST", "/subscribe") => {
            let req: SubscribeReq = serde_json::from_slice(&body)?;
            let repo_path = ensure_repo(staging, &req.owner, &req.repo)?;
            subs.upsert(req.uuid, req.owner, req.repo, req.pr_sync, req.notifications);
            respond_200(&mut stream, &serde_json::json!({"path": repo_path}).to_string())
        }
        ("POST", "/heartbeat") => {
            let req: HeartbeatReq = serde_json::from_slice(&body)?;
            let ok = subs.heartbeat(&req.uuid);
            respond_200(&mut stream, &serde_json::json!({"ok": ok}).to_string())
        }
        ("POST", "/pause") => {
            paused.store(true, Ordering::Relaxed);
            respond_200(&mut stream, r#"{"ok":true}"#)
        }
        ("POST", "/resume") => {
            paused.store(false, Ordering::Relaxed);
            respond_200(&mut stream, r#"{"ok":true}"#)
        }
        _ => {
            stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n")?;
            Ok(())
        }
    }
}

fn respond_200(stream: &mut TcpStream, body: &str) -> Result<()> {
    let resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
        body.len(),
        body
    );
    stream.write_all(resp.as_bytes())?;
    Ok(())
}

/// Clone repo to `{staging}/{owner}/{repo}` if absent, then `git fetch --all`.
fn ensure_repo(staging: &Path, owner: &str, repo: &str) -> Result<PathBuf> {
    let dest = staging.join(owner).join(repo);
    if !dest.exists() {
        std::fs::create_dir_all(dest.parent().unwrap())?;
        let slug = format!("{owner}/{repo}");
        let status = Command::new("gh")
            .args(["repo", "clone", &slug, &dest.to_string_lossy()])
            .status()
            .map_err(|e| anyhow::anyhow!("gh repo clone: {e}"))?;
        if !status.success() {
            anyhow::bail!("gh repo clone {slug} failed");
        }
        tracing::info!(%slug, path = %dest.display(), "cloned");
    } else {
        let status = Command::new("git")
            .args(["-C", &dest.to_string_lossy(), "fetch", "--all"])
            .status()
            .map_err(|e| anyhow::anyhow!("git fetch: {e}"))?;
        if !status.success() {
            anyhow::bail!("git fetch --all failed in {}", dest.display());
        }
        tracing::debug!(path = %dest.display(), "fetched");
    }
    Ok(dest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subscriptions_upsert_and_heartbeat() {
        let subs = Subscriptions::new(Duration::from_secs(30));
        subs.upsert("u1".into(), "myorg".into(), "backend".into(), false, false);
        assert!(subs.heartbeat("u1"));
        assert!(!subs.heartbeat("u2"));
    }

    #[test]
    fn subscriptions_active_pr_sync_repos() {
        let subs = Subscriptions::new(Duration::from_secs(30));
        subs.upsert("u1".into(), "myorg".into(), "backend".into(), false, false);
        subs.upsert("u2".into(), "myorg".into(), "backend".into(), true, false);
        subs.upsert("u3".into(), "myorg".into(), "frontend".into(), false, false);

        let repos = subs.active_pr_sync_repos();
        assert_eq!(repos.len(), 1);
        assert_eq!(repos[0], ("myorg".into(), "backend".into()));
    }

    #[test]
    fn subscriptions_pr_sync_upgrades_on_upsert() {
        let subs = Subscriptions::new(Duration::from_secs(30));
        subs.upsert("u1".into(), "myorg".into(), "backend".into(), false, false);
        assert!(subs.active_pr_sync_repos().is_empty());
        subs.upsert("u1".into(), "myorg".into(), "backend".into(), true, false);
        assert_eq!(subs.active_pr_sync_repos().len(), 1);
    }

    #[test]
    fn subscriptions_sweep_removes_expired() {
        let subs = Subscriptions::new(Duration::from_nanos(1));
        subs.upsert("u1".into(), "myorg".into(), "backend".into(), true, false);
        std::thread::sleep(Duration::from_millis(1));
        subs.sweep();
        assert!(subs.active_pr_sync_repos().is_empty());
    }
}
