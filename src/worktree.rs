// Local git-worktree discovery. A filesystem walk under configured roots finds
// repo clones; `git worktree list --porcelain` enumerates each clone's worktrees;
// a bounded-concurrency `git status` sweep marks each dirty/clean. Results are
// diffed against the `worktree` table and every delta is written to `change_log`
// so the existing /events SSE broadcasts it. Sole writer of the `worktree` table.
//
// This exists so UIs (instant's worktree panel) read a snapshot + subscribe to
// pushes instead of shelling out to git hundreds of times on their hot path.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use serde::Serialize;
use sqlx::{Row, SqlitePool};
use tokio::process::Command;
use tokio::sync::Semaphore;

const EXTRA_PATH: &str = "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin";

// Heavy/uninteresting dirs we never descend into. NOT .worktrees — that's where
// worktrees often live.
const SKIP: &[&str] = &[
    "node_modules", "target", ".git", "Library", ".cache", ".cargo", ".rustup",
    ".npm", ".Trash", "dist", "build", ".next", "vendor", "Pictures", "Music",
];

const MAX_DEPTH: usize = 4;

/// One local worktree. Field set + order mirror instant's `WorktreeRow` so the
/// JSON snapshot and SSE payloads drop straight into its store.
#[derive(Serialize, Clone, PartialEq)]
pub struct WorktreeRow {
    pub origin: String,   // remote.origin.url — groups clones of the same repo
    pub clone: String,    // the clone's main worktree path
    pub worktree: String, // this worktree's path
    pub branch: String,   // branch name, or (detached)/(bare)
    pub head: String,     // short sha
    pub is_main: bool,    // primary worktree vs linked
    pub dirty: bool,      // has uncommitted changes
}

async fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let path = match std::env::var("PATH") {
        Ok(p) => format!("{EXTRA_PATH}:{p}"),
        Err(_) => EXTRA_PATH.to_string(),
    };
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("PATH", path)
        .output()
        .await
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

fn is_repo(dir: &Path) -> bool {
    dir.join(".git").exists()
}

// Collect git locations. A repo stops recursion (its worktrees come from git,
// not the filesystem walk), keeping this fast.
fn walk(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if depth > MAX_DEPTH {
        return;
    }
    if is_repo(dir) {
        out.push(dir.to_path_buf());
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let p = e.path();
        if !p.is_dir() {
            continue;
        }
        let name = e.file_name();
        if SKIP.contains(&name.to_string_lossy().as_ref()) {
            continue;
        }
        walk(&p, depth + 1, out);
    }
}

// A worktree enumerated from porcelain, before its dirty flag is computed.
struct Pending {
    origin: String,
    clone: String,
    worktree: String,
    branch: String,
    head: String,
    is_main: bool,
}

/// Full scan: walk `roots`, dedupe clones by git-common-dir, enumerate worktrees,
/// then compute `dirty` for each in parallel capped at `cap` concurrent processes.
pub async fn scan(roots: &[PathBuf], cap: usize) -> Vec<WorktreeRow> {
    let mut candidates = Vec::new();
    for r in roots {
        walk(r, 0, &mut candidates);
    }

    // Dedupe clones by canonical git-common-dir; keep one probe dir per clone.
    let mut clones: BTreeMap<String, PathBuf> = BTreeMap::new();
    for c in candidates {
        let Ok(common) = git(&c, &["rev-parse", "--git-common-dir"]).await else { continue };
        let abs = if Path::new(&common).is_absolute() {
            PathBuf::from(&common)
        } else {
            c.join(&common)
        };
        let key = std::fs::canonicalize(&abs)
            .unwrap_or(abs)
            .to_string_lossy()
            .to_string();
        clones.entry(key).or_insert(c);
    }

    // Enumerate worktrees per clone (cheap, sequential git calls).
    let mut pending: Vec<Pending> = Vec::new();
    for probe in clones.values() {
        let Ok(porcelain) = git(probe, &["worktree", "list", "--porcelain"]).await else { continue };
        let blocks: Vec<&str> = porcelain.split("\n\n").filter(|b| !b.trim().is_empty()).collect();
        let main_path = blocks
            .first()
            .and_then(|b| b.lines().find_map(|l| l.strip_prefix("worktree ")))
            .unwrap_or("")
            .to_string();
        let origin = git(Path::new(&main_path), &["config", "--get", "remote.origin.url"])
            .await
            .unwrap_or_default();

        for block in blocks {
            let mut wt = String::new();
            let mut head = String::new();
            let mut branch = String::new();
            for line in block.lines() {
                if let Some(v) = line.strip_prefix("worktree ") {
                    wt = v.to_string();
                } else if let Some(v) = line.strip_prefix("HEAD ") {
                    head = v.chars().take(8).collect();
                } else if let Some(v) = line.strip_prefix("branch ") {
                    branch = v.trim_start_matches("refs/heads/").to_string();
                } else if line == "detached" {
                    branch = "(detached)".to_string();
                } else if line == "bare" {
                    branch = "(bare)".to_string();
                }
            }
            if wt.is_empty() {
                continue;
            }
            pending.push(Pending {
                origin: origin.clone(),
                clone: main_path.clone(),
                is_main: wt == main_path,
                worktree: wt,
                branch,
                head,
            });
        }
    }

    // Dirty check is the expensive part: cap concurrent `git status` subprocesses
    // so a few-hundred-worktree sweep doesn't fork a few-hundred at once.
    let sem = Arc::new(Semaphore::new(cap.max(1)));
    let mut handles = Vec::with_capacity(pending.len());
    for p in pending {
        let sem = sem.clone();
        handles.push(tokio::spawn(async move {
            let _permit = sem.acquire_owned().await.ok()?;
            let dirty = git(Path::new(&p.worktree), &["status", "--porcelain"])
                .await
                .map(|s| !s.is_empty())
                .unwrap_or(false);
            Some(WorktreeRow {
                origin: p.origin,
                clone: p.clone,
                worktree: p.worktree,
                branch: p.branch,
                head: p.head,
                is_main: p.is_main,
                dirty,
            })
        }));
    }

    let mut rows = Vec::new();
    for h in handles {
        if let Ok(Some(row)) = h.await {
            rows.push(row);
        }
    }
    rows
}

// Read the current table into a path-keyed map of (id, row).
async fn load_existing(pool: &SqlitePool) -> Result<HashMap<String, (i64, WorktreeRow)>> {
    let rows = sqlx::query(
        "SELECT id, origin, clone_path, path, branch, head, is_main, dirty FROM worktree",
    )
    .fetch_all(pool)
    .await?;
    let mut map = HashMap::with_capacity(rows.len());
    for r in &rows {
        let id: i64 = r.get(0);
        let row = WorktreeRow {
            origin: r.get::<Option<String>, _>(1).unwrap_or_default(),
            clone: r.get(2),
            worktree: r.get(3),
            branch: r.get(4),
            head: r.get(5),
            is_main: r.get::<i64, _>(6) != 0,
            dirty: r.get::<i64, _>(7) != 0,
        };
        map.insert(row.worktree.clone(), (id, row));
    }
    Ok(map)
}

/// Scan once, reconcile the `worktree` table, and log each insert/update/delete
/// to `change_log` (which the SSE broadcasts). Returns the number of deltas.
pub async fn sync_once(pool: &SqlitePool, roots: &[PathBuf], cap: usize) -> Result<usize> {
    let scanned = scan(roots, cap).await;
    let mut existing = load_existing(pool).await?;
    let mut conn = pool.acquire().await?;
    let mut deltas = 0usize;
    let mut seen: HashSet<String> = HashSet::with_capacity(scanned.len());

    for row in &scanned {
        seen.insert(row.worktree.clone());
        match existing.get(&row.worktree) {
            Some((_, old)) if old == row => {} // unchanged
            Some((id, _)) => {
                let id = *id;
                sqlx::query(
                    "UPDATE worktree SET origin=?, clone_path=?, branch=?, head=?, is_main=?, dirty=?,
                     scanned_at=strftime('%Y-%m-%dT%H:%M:%SZ','now') WHERE id=?",
                )
                .bind(&row.origin)
                .bind(&row.clone)
                .bind(&row.branch)
                .bind(&row.head)
                .bind(row.is_main as i64)
                .bind(row.dirty as i64)
                .bind(id)
                .execute(&mut *conn)
                .await?;
                let payload = serde_json::to_value(row)?;
                crate::db::log_change(&mut conn, "worktree", id, crate::db::ChangeEvent::Updated, None, Some(&payload)).await?;
                deltas += 1;
            }
            None => {
                let id = sqlx::query(
                    "INSERT INTO worktree (origin, clone_path, path, branch, head, is_main, dirty)
                     VALUES (?, ?, ?, ?, ?, ?, ?)",
                )
                .bind(&row.origin)
                .bind(&row.clone)
                .bind(&row.worktree)
                .bind(&row.branch)
                .bind(&row.head)
                .bind(row.is_main as i64)
                .bind(row.dirty as i64)
                .execute(&mut *conn)
                .await?
                .last_insert_rowid();
                let payload = serde_json::to_value(row)?;
                crate::db::log_change(&mut conn, "worktree", id, crate::db::ChangeEvent::Inserted, None, Some(&payload)).await?;
                deltas += 1;
            }
        }
        existing.remove(&row.worktree);
    }

    // Anything left in `existing` was not seen this scan -> gone.
    for (path, (id, _)) in existing {
        if seen.contains(&path) {
            continue;
        }
        sqlx::query("DELETE FROM worktree WHERE id=?")
            .bind(id)
            .execute(&mut *conn)
            .await?;
        let payload = serde_json::json!({ "worktree": path });
        crate::db::log_change(&mut conn, "worktree", id, crate::db::ChangeEvent::Deleted, None, Some(&payload)).await?;
        deltas += 1;
    }

    if deltas > 0 {
        tracing::info!(deltas, total = scanned.len(), "worktree: synced");
    }
    Ok(deltas)
}

/// Background loop: initial scan, then rescan on a debounced filesystem event or
/// a periodic fallback tick, whichever comes first. Spawned once from `watch`.
pub async fn watch_loop(pool: SqlitePool, roots: Vec<PathBuf>, interval: Duration, cap: usize) {
    if let Err(e) = sync_once(&pool, &roots, cap).await {
        tracing::warn!(error = %e, "worktree: initial scan failed");
    }

    // notify runs its callback on its own thread; forward a coalescing tick into
    // a tokio channel so the async loop can debounce and rescan.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    let watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if res.is_ok() {
            let _ = tx.send(());
        }
    });
    let mut watcher = match watcher {
        Ok(w) => w,
        Err(e) => {
            tracing::warn!(error = %e, "worktree: fs watcher unavailable; periodic rescan only");
            // Fall back to interval-only.
            let mut ticker = tokio::time::interval(interval);
            loop {
                ticker.tick().await;
                if let Err(e) = sync_once(&pool, &roots, cap).await {
                    tracing::warn!(error = %e, "worktree: rescan failed");
                }
            }
        }
    };
    for root in &roots {
        use notify::Watcher;
        if let Err(e) = watcher.watch(root, notify::RecursiveMode::Recursive) {
            tracing::warn!(root = %root.display(), error = %e, "worktree: watch failed");
        }
    }

    let debounce = Duration::from_millis(1500);
    let mut ticker = tokio::time::interval(interval);
    loop {
        tokio::select! {
            _ = rx.recv() => {
                // Coalesce the burst: wait out the debounce, drain everything queued.
                tokio::time::sleep(debounce).await;
                while rx.try_recv().is_ok() {}
            }
            _ = ticker.tick() => {}
        }
        if let Err(e) = sync_once(&pool, &roots, cap).await {
            tracing::warn!(error = %e, "worktree: rescan failed");
        }
    }
}
