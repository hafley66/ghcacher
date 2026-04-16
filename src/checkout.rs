use anyhow::{anyhow, bail, Context, Result};
use sqlx::SqlitePool;
use std::path::Path;
use tokio::process::Command;

pub struct CheckoutTask {
    pub owner: String,
    pub name: String,
    /// Directory name for owner on disk (may differ from `owner` when fs_alias is set).
    pub fs_owner: String,
    /// Some activity was observed for this repo this cycle (dirty PR or branch push).
    /// Absent the dir, a clone happens regardless; otherwise this gates the fetch.
    pub is_dirty: bool,
}

/// CLI-driven explicit checkout: clone if missing, fetch, reset HEAD to `branch`.
pub async fn checkout_one(
    pool: &SqlitePool,
    staging: &Path,
    owner: &str,
    name: &str,
    branch: &str,
    fs_owner: Option<&str>,
) -> Result<()> {
    let mut conn = pool.acquire().await?;
    let _ = crate::db::get_repo_id(&mut *conn, owner, name).await?
        .ok_or_else(|| anyhow!("repo {owner}/{name} not in DB; run sync first"))?;
    drop(conn);

    let fs_owner = fs_owner.unwrap_or(owner);
    let local_path = staging.join(fs_owner).join(name);
    let slug = format!("{owner}/{name}");

    if !local_path.exists() {
        run_clone(&slug, &local_path).await?;
    } else {
        run_fetch(&local_path).await?;
    }
    run_reset(&local_path, branch).await
}

/// Watch-driven fetch sweep: clone missing dirs, fetch origin for dirty repos,
/// skip everything else. HEAD is never touched.
pub async fn checkout_all(
    staging: &Path,
    tasks: &[CheckoutTask],
) -> Result<()> {
    let mut handles: Vec<tokio::task::JoinHandle<(String, Result<()>)>> = vec![];
    for t in tasks {
        let staging = staging.to_path_buf();
        let owner = t.owner.clone();
        let name = t.name.clone();
        let fs_owner = t.fs_owner.clone();
        let is_dirty = t.is_dirty;
        let slug = format!("{owner}/{name}");
        handles.push(tokio::spawn(async move {
            let local_path = staging.join(&fs_owner).join(&name);
            let res = if !local_path.exists() {
                run_clone(&slug, &local_path).await
            } else if is_dirty {
                run_fetch(&local_path).await
            } else {
                tracing::debug!(%slug, "checkout: no new activity, skipping");
                Ok(())
            };
            (slug, res)
        }));
    }
    for h in handles {
        let (slug, res) = h.await.map_err(|_| anyhow!("checkout task panicked"))?;
        if let Err(e) = res {
            tracing::error!(%slug, error = %e, "checkout: repo failed");
        }
    }
    Ok(())
}

async fn run_clone(slug: &str, dest: &Path) -> Result<()> {
    if let Some(parent) = dest.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let status = Command::new("gh")
        .args(["repo", "clone", slug, &dest.to_string_lossy()])
        .status()
        .await
        .context("gh repo clone")?;
    if !status.success() {
        bail!("gh repo clone {slug} exited {status}");
    }
    tracing::info!(%slug, path = %dest.display(), "cloned");
    Ok(())
}

async fn run_fetch(path: &Path) -> Result<()> {
    let path_str = path.to_string_lossy();
    let status = Command::new("git")
        .args(["-C", &path_str, "fetch", "origin"])
        .status()
        .await
        .context("git fetch")?;
    if !status.success() {
        bail!("git fetch failed in {}", path.display());
    }
    tracing::info!(path = %path.display(), "fetched origin");
    Ok(())
}

async fn run_reset(path: &Path, branch: &str) -> Result<()> {
    let path_str = path.to_string_lossy();
    let remote_ref = format!("origin/{branch}");
    let status = Command::new("git")
        .args(["-C", &path_str, "reset", "--hard", &remote_ref])
        .status()
        .await
        .context("git reset --hard")?;
    if !status.success() {
        bail!("git reset --hard {remote_ref} failed in {}", path.display());
    }
    tracing::info!(path = %path.display(), %branch, "reset --hard to remote");
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
