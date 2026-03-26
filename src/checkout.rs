use anyhow::{anyhow, bail, Context, Result};
use sqlx::{Row, SqlitePool};
use std::path::{Path, PathBuf};
use tokio::process::Command;

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

pub async fn checkout_one(
    pool: &SqlitePool,
    staging: &Path,
    owner: &str,
    name: &str,
    branch: &str,
) -> Result<()> {
    let mut conn = pool.acquire().await?;
    let repo_id = crate::db::get_repo_id(&mut *conn, owner, name).await?
        .ok_or_else(|| anyhow!("repo {owner}/{name} not in DB; run sync first"))?;
    drop(conn);
    checkout_all(pool, staging, &[CheckoutTask {
        repo_id,
        owner: owner.into(),
        name: name.into(),
        branch: branch.into(),
    }]).await
}

pub async fn checkout_all(
    pool: &SqlitePool,
    staging: &Path,
    tasks: &[CheckoutTask],
) -> Result<()> {
    let unique_repo_ids: Vec<i64> = {
        use std::collections::HashSet;
        tasks.iter().map(|t| t.repo_id).collect::<HashSet<_>>().into_iter().collect()
    };

    if unique_repo_ids.is_empty() {
        return Ok(());
    }

    let mut qb = sqlx::QueryBuilder::new(
        "SELECT b.repo_id, b.name, b.sha, c.sha
         FROM branch b
         LEFT JOIN checkout c ON c.repo_id = b.repo_id AND c.branch = b.name
         WHERE b.repo_id IN (",
    );
    let mut sep = qb.separated(", ");
    for id in &unique_repo_ids {
        sep.push_bind(*id);
    }
    sep.push_unseparated(")");

    let rows = qb.build().fetch_all(pool).await?;
    let sha_map: std::collections::HashMap<(i64, String), (Option<String>, Option<String>)> = rows
        .iter()
        .map(|row| {
            let repo_id: i64 = row.get(0);
            let branch: String = row.get(1);
            let branch_sha: Option<String> = row.get(2);
            let checkout_sha: Option<String> = row.get(3);
            ((repo_id, branch), (branch_sha, checkout_sha))
        })
        .collect();

    let items: Vec<WorkItem> = tasks.iter().map(|t| {
        let local_path = staging.join(&t.owner).join(&t.name);
        let (new_sha, stored_sha) = sha_map
            .get(&(t.repo_id, t.branch.clone()))
            .cloned()
            .unwrap_or((None, None));
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

    type TaskResult = Result<(i64, String, PathBuf, Option<String>)>;

    let mut handles: Vec<tokio::task::JoinHandle<TaskResult>> = vec![];
    for item in items {
        let WorkItem { repo_id, owner, name, branch, local_path, new_sha, op } = item;
        match op {
            Op::Skip => {
                tracing::debug!(path = %local_path.display(), "checkout: sha unchanged, skipping");
            }
            Op::Clone => {
                let slug = format!("{owner}/{name}");
                handles.push(tokio::spawn(async move {
                    run_clone(&slug, &branch, &local_path).await?;
                    Ok((repo_id, branch, local_path, new_sha))
                }));
            }
            Op::Fetch => {
                handles.push(tokio::spawn(async move {
                    run_fetch(&local_path, &branch).await?;
                    Ok((repo_id, branch, local_path, new_sha))
                }));
            }
        }
    }

    for handle in handles {
        match handle.await.map_err(|_| anyhow!("checkout task panicked"))? {
            Ok((repo_id, branch, local_path, new_sha)) => {
                let now = chrono::Utc::now().to_rfc3339();
                sqlx::query(
                    "INSERT INTO checkout (repo_id, branch, local_path, sha, checked_out_at)
                     VALUES (?, ?, ?, ?, ?)
                     ON CONFLICT(repo_id, branch) DO UPDATE SET
                         local_path     = excluded.local_path,
                         sha            = excluded.sha,
                         checked_out_at = excluded.checked_out_at",
                )
                .bind(repo_id)
                .bind(&branch)
                .bind(local_path.to_string_lossy().as_ref())
                .bind(&new_sha)
                .bind(&now)
                .execute(pool)
                .await?;
                tracing::info!(path = %local_path.display(), "checkout: done");
            }
            Err(e) => {
                tracing::error!(error = %e, "checkout: git op failed");
            }
        }
    }

    Ok(())
}

async fn run_clone(slug: &str, branch: &str, dest: &Path) -> Result<()> {
    if let Some(parent) = dest.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let status = Command::new("gh")
        .args(["repo", "clone", slug, &dest.to_string_lossy(), "--", "--branch", branch])
        .status()
        .await
        .context("gh repo clone")?;
    if !status.success() {
        bail!("gh repo clone {slug} --branch {branch} exited {status}");
    }
    tracing::info!(%slug, %branch, path = %dest.display(), "cloned");
    Ok(())
}

async fn run_fetch(path: &Path, branch: &str) -> Result<()> {
    let path_str = path.to_string_lossy();

    let fetch = Command::new("git")
        .args(["-C", &path_str, "fetch", "origin", branch])
        .status()
        .await
        .context("git fetch")?;
    if !fetch.success() {
        bail!("git fetch origin {branch} failed in {}", path.display());
    }

    let reset = Command::new("git")
        .args(["-C", &path_str, "reset", "--hard", "FETCH_HEAD"])
        .status()
        .await
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
