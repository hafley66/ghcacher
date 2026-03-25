use anyhow::{anyhow, bail, Context, Result};
use rusqlite::{Connection, params};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;

pub struct CheckoutTask {
    pub repo_id: i64,
    pub owner: String,
    pub name: String,
    pub branch: String,
}

enum Op {
    Clone,
    Fetch,
    Skip,
}

struct WorkItem {
    repo_id: i64,
    owner: String,
    name: String,
    branch: String,
    local_path: PathBuf,
    new_sha: Option<String>,
    op: Op,
}

pub fn checkout_one(
    conn: &Connection,
    staging: &Path,
    owner: &str,
    name: &str,
    branch: &str,
) -> Result<()> {
    let repo_id = crate::db::get_repo_id(conn, owner, name)?
        .ok_or_else(|| anyhow!("repo {owner}/{name} not in DB; run sync first"))?;
    checkout_all(conn, staging, &[CheckoutTask {
        repo_id,
        owner: owner.into(),
        name: name.into(),
        branch: branch.into(),
    }])
}

pub fn checkout_all(
    conn: &Connection,
    staging: &Path,
    tasks: &[CheckoutTask],
) -> Result<()> {
    let items: Vec<WorkItem> = tasks.iter().map(|t| {
        let local_path = staging.join(&t.owner).join(&t.branch).join(&t.name);

        let new_sha: Option<String> = conn.query_row(
            "SELECT sha FROM branch WHERE repo_id=?1 AND name=?2",
            params![t.repo_id, t.branch],
            |r| r.get(0),
        ).ok().flatten();

        let stored_sha: Option<String> = conn.query_row(
            "SELECT sha FROM checkout WHERE repo_id=?1 AND branch=?2",
            params![t.repo_id, t.branch],
            |r| r.get(0),
        ).ok().flatten();

        let op = if new_sha.is_some() && new_sha == stored_sha {
            Op::Skip
        } else if local_path.exists() {
            Op::Fetch
        } else {
            Op::Clone
        };

        WorkItem {
            repo_id: t.repo_id,
            owner: t.owner.clone(),
            name: t.name.clone(),
            branch: t.branch.clone(),
            local_path,
            new_sha,
            op,
        }
    }).collect();

    type ThreadResult = Result<(i64, String, PathBuf, Option<String>)>;

    let handles: Vec<thread::JoinHandle<ThreadResult>> = items
        .into_iter()
        .filter_map(|item| {
            let WorkItem { repo_id, owner, name, branch, local_path, new_sha, op } = item;
            match op {
                Op::Skip => {
                    tracing::debug!(
                        path = %local_path.display(),
                        "checkout: sha unchanged, skipping"
                    );
                    None
                }
                Op::Clone => {
                    let slug = format!("{owner}/{name}");
                    Some(thread::spawn(move || -> ThreadResult {
                        run_clone(&slug, &branch, &local_path)?;
                        Ok((repo_id, branch, local_path, new_sha))
                    }))
                }
                Op::Fetch => {
                    Some(thread::spawn(move || -> ThreadResult {
                        run_fetch(&local_path, &branch)?;
                        Ok((repo_id, branch, local_path, new_sha))
                    }))
                }
            }
        })
        .collect();

    for handle in handles {
        match handle.join().map_err(|_| anyhow!("checkout thread panicked"))? {
            Ok((repo_id, branch, local_path, new_sha)) => {
                let now = chrono::Utc::now().to_rfc3339();
                conn.execute(
                    "INSERT INTO checkout (repo_id, branch, local_path, sha, checked_out_at)
                     VALUES (?1, ?2, ?3, ?4, ?5)
                     ON CONFLICT(repo_id, branch) DO UPDATE SET
                         local_path     = excluded.local_path,
                         sha            = excluded.sha,
                         checked_out_at = excluded.checked_out_at",
                    params![
                        repo_id,
                        branch,
                        local_path.to_string_lossy(),
                        new_sha,
                        now
                    ],
                )?;
                tracing::info!(path = %local_path.display(), "checkout: done");
            }
            Err(e) => {
                tracing::error!(error = %e, "checkout: git op failed");
            }
        }
    }

    Ok(())
}

fn run_clone(slug: &str, branch: &str, dest: &Path) -> Result<()> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let status = Command::new("gh")
        .args(["repo", "clone", slug, &dest.to_string_lossy(), "--", "--branch", branch])
        .status()
        .context("gh repo clone")?;
    if !status.success() {
        bail!("gh repo clone {slug} --branch {branch} exited {status}");
    }
    tracing::info!(%slug, %branch, path = %dest.display(), "cloned");
    Ok(())
}

fn run_fetch(path: &Path, branch: &str) -> Result<()> {
    let path_str = path.to_string_lossy();

    let fetch = Command::new("git")
        .args(["-C", &path_str, "fetch", "origin", branch])
        .status()
        .context("git fetch")?;
    if !fetch.success() {
        bail!("git fetch origin {branch} failed in {}", path.display());
    }

    let reset = Command::new("git")
        .args(["-C", &path_str, "reset", "--hard", "FETCH_HEAD"])
        .status()
        .context("git reset --hard")?;
    if !reset.success() {
        bail!("git reset --hard failed in {}", path.display());
    }

    tracing::info!(path = %path.display(), %branch, "fetched and reset");
    Ok(())
}

pub fn parse_slug(slug: &str) -> Result<(String, String)> {
    let mut parts = slug.splitn(2, '/');
    let owner = parts.next().filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("invalid repo slug (expected owner/name): {slug}"))?;
    let name = parts.next().filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("invalid repo slug (expected owner/name): {slug}"))?;
    Ok((owner.into(), name.into()))
}
