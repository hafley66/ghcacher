use anyhow::Result;
use serde::Deserialize;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

pub const DEFAULT_PORT: u16 = 7748;
pub const DEFAULT_TTL_SECS: u64 = 30;

#[derive(Deserialize)]
struct SubscribeReq {
    uuid: String,
    owner: String,
    repo: String,
    #[serde(default)]
    pr_sync: bool,
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

    fn upsert(&self, uuid: String, owner: String, repo: String, pr_sync: bool) {
        let mut g = self.inner.write().unwrap();
        let sub = g.entry(uuid).or_insert_with(|| Sub {
            last_seen: Instant::now(),
            repos: vec![],
        });
        sub.last_seen = Instant::now();
        match sub.repos.iter_mut().find(|r| r.owner == owner && r.repo == repo) {
            Some(r) => r.pr_sync = r.pr_sync || pr_sync,
            None => sub.repos.push(RepoSub { owner, repo, pr_sync }),
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

    /// All (owner, repo) pairs that have at least one live subscriber with pr_sync=true.
    pub fn active_pr_sync_repos(&self) -> Vec<(String, String)> {
        let ttl = self.ttl;
        let g = self.inner.read().unwrap();
        let mut set = std::collections::HashSet::new();
        for sub in g.values() {
            if sub.last_seen.elapsed() < ttl {
                for r in &sub.repos {
                    if r.pr_sync {
                        set.insert((r.owner.clone(), r.repo.clone()));
                    }
                }
            }
        }
        set.into_iter().collect()
    }
}

pub fn run(subs: Arc<Subscriptions>, staging: PathBuf, port: u16) -> Result<()> {
    let listener = TcpListener::bind(format!("127.0.0.1:{port}"))
        .map_err(|e| anyhow::anyhow!("cmd HTTP bind 127.0.0.1:{port}: {e}"))?;
    tracing::info!(port, "cmd server listening on 127.0.0.1");

    let subs_sweep = Arc::clone(&subs);
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_secs(10));
        subs_sweep.sweep();
    });

    for stream in listener.incoming() {
        match stream {
            Ok(s) => {
                let subs = Arc::clone(&subs);
                let staging = staging.clone();
                std::thread::spawn(move || {
                    if let Err(e) = handle(s, &subs, &staging) {
                        tracing::warn!(error = %e, "cmd: request failed");
                    }
                });
            }
            Err(e) => tracing::warn!(error = %e, "cmd: accept error"),
        }
    }
    Ok(())
}

fn handle(mut stream: TcpStream, subs: &Subscriptions, staging: &Path) -> Result<()> {
    let mut reader = BufReader::new(&stream);

    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    let parts: Vec<&str> = request_line.trim().splitn(3, ' ').collect();
    if parts.len() < 2 {
        return Ok(());
    }
    let (method, path) = (parts[0], parts[1]);

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

    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body)?;
    }

    match (method, path) {
        ("POST", "/subscribe") => {
            let req: SubscribeReq = serde_json::from_slice(&body)?;
            let repo_path = ensure_repo(staging, &req.owner, &req.repo)?;
            subs.upsert(req.uuid, req.owner, req.repo, req.pr_sync);
            respond_200(
                &mut stream,
                &serde_json::json!({"path": repo_path}).to_string(),
            )
        }
        ("POST", "/heartbeat") => {
            let req: HeartbeatReq = serde_json::from_slice(&body)?;
            let ok = subs.heartbeat(&req.uuid);
            respond_200(&mut stream, &serde_json::json!({"ok": ok}).to_string())
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
        subs.upsert("u1".into(), "myorg".into(), "backend".into(), false);
        assert!(subs.heartbeat("u1"));
        assert!(!subs.heartbeat("u2"));
    }

    #[test]
    fn subscriptions_active_pr_sync_repos() {
        let subs = Subscriptions::new(Duration::from_secs(30));
        subs.upsert("u1".into(), "myorg".into(), "backend".into(), false);
        subs.upsert("u2".into(), "myorg".into(), "backend".into(), true);
        subs.upsert("u3".into(), "myorg".into(), "frontend".into(), false);

        let repos = subs.active_pr_sync_repos();
        assert_eq!(repos.len(), 1);
        assert_eq!(repos[0], ("myorg".into(), "backend".into()));
    }

    #[test]
    fn subscriptions_pr_sync_upgrades_on_upsert() {
        let subs = Subscriptions::new(Duration::from_secs(30));
        subs.upsert("u1".into(), "myorg".into(), "backend".into(), false);
        assert!(subs.active_pr_sync_repos().is_empty());
        // Same uuid upgrades pr_sync for the same repo
        subs.upsert("u1".into(), "myorg".into(), "backend".into(), true);
        assert_eq!(subs.active_pr_sync_repos().len(), 1);
    }

    #[test]
    fn subscriptions_sweep_removes_expired() {
        let subs = Subscriptions::new(Duration::from_nanos(1));
        subs.upsert("u1".into(), "myorg".into(), "backend".into(), true);
        std::thread::sleep(Duration::from_millis(1));
        subs.sweep();
        assert!(subs.active_pr_sync_repos().is_empty());
    }
}
