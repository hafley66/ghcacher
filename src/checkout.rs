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
    /// The default branch for this repo as known by the DB (e.g. "main").
    pub default_branch: String,
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

/// Watch-driven checkout sweep: clone missing dirs, fetch origin for dirty repos,
/// or pull when on the default branch with a clean working tree.
///
/// Decision matrix for an existing clone:
/// - is_dirty == false          -> skip
/// - is_dirty == true + on default branch + working tree clean -> git pull
/// - is_dirty == true + otherwise                              -> git fetch origin
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
        let default_branch = t.default_branch.clone();
        let slug = format!("{owner}/{name}");
        handles.push(tokio::spawn(async move {
            let local_path = staging.join(&fs_owner).join(&name);
            let res = if !local_path.exists() {
                run_clone(&slug, &local_path).await
            } else if is_dirty {
                match should_pull(&local_path, &default_branch).await {
                    Ok(true) => run_pull(&local_path).await,
                    Ok(false) => run_fetch(&local_path).await,
                    Err(e) => {
                        tracing::warn!(%slug, error = %e, "could not determine pull eligibility, falling back to fetch");
                        run_fetch(&local_path).await
                    }
                }
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

/// Returns true only when the current branch matches `default_branch` and the
/// working tree has no modified, staged, or untracked files.
async fn should_pull(path: &Path, default_branch: &str) -> Result<bool> {
    let current = git_current_branch(path).await?;
    if current != default_branch {
        tracing::debug!(
            path = %path.display(),
            current_branch = %current,
            default_branch = %default_branch,
            "not on default branch, will fetch"
        );
        return Ok(false);
    }
    let clean = git_working_tree_clean(path).await?;
    if !clean {
        tracing::debug!(
            path = %path.display(),
            "working tree is not clean, will fetch"
        );
    }
    Ok(clean)
}

/// Empty output from `git status --porcelain` means nothing modified, staged, or untracked.
async fn git_working_tree_clean(path: &Path) -> Result<bool> {
    let output = Command::new("git")
        .args(["-C", &path.to_string_lossy(), "status", "--porcelain"])
        .output()
        .await
        .context("git status --porcelain")?;
    if !output.status.success() {
        bail!(
            "git status --porcelain failed in {}: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(output.stdout.is_empty())
}

/// Returns the current branch name, or an empty string when in detached HEAD.
async fn git_current_branch(path: &Path) -> Result<String> {
    let output = Command::new("git")
        .args(["-C", &path.to_string_lossy(), "branch", "--show-current"])
        .output()
        .await
        .context("git branch --show-current")?;
    if !output.status.success() {
        bail!(
            "git branch --show-current failed in {}: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
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

async fn run_pull(path: &Path) -> Result<()> {
    let path_str = path.to_string_lossy();
    let status = Command::new("git")
        .args(["-C", &path_str, "pull", "--ff-only"])
        .status()
        .await
        .context("git pull --ff-only")?;
    if !status.success() {
        bail!("git pull --ff-only failed in {}", path.display());
    }
    tracing::info!(path = %path.display(), "pulled origin --ff-only");
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    async fn git_cmd(path: &Path, args: &[&str]) -> std::process::Output {
        Command::new("git")
            .args(std::iter::once("-C").chain(std::iter::once(path.to_str().unwrap())).chain(args.iter().copied()))
            .output()
            .await
            .unwrap_or_else(|e| panic!("git {:?} failed: {e}", args))
    }

    async fn git_ok(path: &Path, args: &[&str]) {
        let out = git_cmd(path, args).await;
        assert!(out.status.success(), "git {:?} failed: {}", args, String::from_utf8_lossy(&out.stderr));
    }

    async fn git_rev_parse(path: &Path, rev: &str) -> String {
        let out = git_cmd(path, &["rev-parse", rev]).await;
        assert!(out.status.success(), "git rev-parse {rev} failed");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// Push a new file to `origin` from a fresh clone living in `tmp/clone2`.
    async fn push_new_commit_to_origin(tmp: &Path, origin: &Path, filename: &str, content: &str) {
        let clone2 = tmp.join("clone2");
        git_ok(tmp, &["clone", origin.to_str().unwrap(), "clone2"]).await;
        tokio::fs::write(clone2.join(filename), content).await.unwrap();
        git_ok(&clone2, &["config", "user.email", "test@example.com"]).await;
        git_ok(&clone2, &["config", "user.name", "Test"]).await;
        git_ok(&clone2, &["add", filename]).await;
        git_ok(&clone2, &["commit", "-m", "second"]).await;
        git_ok(&clone2, &["push"]).await;
    }

    /// Create a bare repo at `origin_dir`, clone it into `clone_dir`, create an
    /// initial commit on main, and push it back to origin. Returns origin path.
    async fn setup_origin_and_clone(origin_dir: &Path, clone_dir: &Path) -> PathBuf {
        // Init bare origin from parent so the directory is created by git.
        git_ok(origin_dir.parent().unwrap(), &[
            "init", "--bare",
            origin_dir.file_name().unwrap().to_str().unwrap(),
        ]).await;

        // Clone it into clone_dir (parent must exist, git creates the leaf).
        git_ok(clone_dir.parent().unwrap(), &[
            "clone",
            origin_dir.to_str().unwrap(),
            clone_dir.file_name().unwrap().to_str().unwrap(),
        ]).await;

        git_ok(clone_dir, &["config", "user.email", "test@example.com"]).await;
        git_ok(clone_dir, &["config", "user.name", "Test"]).await;
        git_ok(clone_dir, &["checkout", "-b", "main"]).await;

        let readme = clone_dir.join("README.md");
        tokio::fs::write(&readme, "hello").await.unwrap();
        git_ok(clone_dir, &["add", "README.md"]).await;
        git_ok(clone_dir, &["commit", "-m", "init"]).await;
        git_ok(clone_dir, &["push", "-u", "origin", "main"]).await;
        // Make sure the bare repo advertises main as its default branch so
        // subsequent clones check out main instead of falling back to master.
        git_ok(origin_dir, &["symbolic-ref", "HEAD", "refs/heads/main"]).await;

        origin_dir.to_path_buf()
    }

    #[tokio::test]
    async fn pull_fast_forwards_when_clean_on_default_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let origin = tmp.path().join("origin.git");
        // checkout_all expects staging/fs_owner/name
        let clone = tmp.path().join("o").join("n");
        tokio::fs::create_dir_all(clone.parent().unwrap()).await.unwrap();
        setup_origin_and_clone(&origin, &clone).await;

        let before = git_rev_parse(&clone, "HEAD").await;
        push_new_commit_to_origin(tmp.path(), &origin, "new.txt", "new").await;

        assert!(!clone.join("new.txt").exists(), "clone should not have new.txt before checkout");

        let task = CheckoutTask {
            owner: "o".into(),
            name: "n".into(),
            fs_owner: "o".into(),
            is_dirty: true,
            default_branch: "main".into(),
        };
        checkout_all(tmp.path(), &[task]).await.unwrap();

        let after = git_rev_parse(&clone, "HEAD").await;
        let origin_main = git_rev_parse(&clone, "origin/main").await;
        assert_eq!(after, origin_main, "local main should fast-forward to origin");
        assert_ne!(before, after, "HEAD should have moved after pull");
        assert!(clone.join("new.txt").exists(), "new file should be present after fast-forward");
    }

    #[tokio::test]
    async fn fetch_only_when_untracked_files_present() {
        let tmp = tempfile::tempdir().unwrap();
        let origin = tmp.path().join("origin.git");
        let clone = tmp.path().join("clone");
        setup_origin_and_clone(&origin, &clone).await;

        tokio::fs::write(clone.join("untracked.txt"), "u").await.unwrap();
        let before = git_rev_parse(&clone, "HEAD").await;
        push_new_commit_to_origin(tmp.path(), &origin, "new.txt", "new").await;

        let task = CheckoutTask {
            owner: "o".into(),
            name: "clone".into(),
            fs_owner: ".".into(),
            is_dirty: true,
            default_branch: "main".into(),
        };
        checkout_all(tmp.path(), &[task]).await.unwrap();

        let after = git_rev_parse(&clone, "HEAD").await;
        let origin_main = git_rev_parse(&clone, "origin/main").await;
        assert_ne!(before, origin_main, "origin should have advanced after fetch");
        assert_eq!(before, after, "local HEAD must NOT move when untracked files block pull");
        assert!(!clone.join("new.txt").exists(), "new file must not appear after fetch-only");
    }

    #[tokio::test]
    async fn fetch_only_when_modified_files_present() {
        let tmp = tempfile::tempdir().unwrap();
        let origin = tmp.path().join("origin.git");
        let clone = tmp.path().join("clone");
        setup_origin_and_clone(&origin, &clone).await;

        tokio::fs::write(clone.join("README.md"), "modified").await.unwrap();
        let before = git_rev_parse(&clone, "HEAD").await;
        push_new_commit_to_origin(tmp.path(), &origin, "new.txt", "new").await;

        let task = CheckoutTask {
            owner: "o".into(),
            name: "clone".into(),
            fs_owner: ".".into(),
            is_dirty: true,
            default_branch: "main".into(),
        };
        checkout_all(tmp.path(), &[task]).await.unwrap();

        let after = git_rev_parse(&clone, "HEAD").await;
        let origin_main = git_rev_parse(&clone, "origin/main").await;
        assert_ne!(before, origin_main, "origin should have advanced after fetch");
        assert_eq!(before, after, "local HEAD must NOT move when modified files block pull");
        assert!(!clone.join("new.txt").exists(), "new file must not appear after fetch-only");
    }

    #[tokio::test]
    async fn fetch_only_when_staged_files_present() {
        let tmp = tempfile::tempdir().unwrap();
        let origin = tmp.path().join("origin.git");
        let clone = tmp.path().join("clone");
        setup_origin_and_clone(&origin, &clone).await;

        tokio::fs::write(clone.join("README.md"), "staged").await.unwrap();
        git_ok(&clone, &["add", "README.md"]).await;
        let before = git_rev_parse(&clone, "HEAD").await;
        push_new_commit_to_origin(tmp.path(), &origin, "new.txt", "new").await;

        let task = CheckoutTask {
            owner: "o".into(),
            name: "clone".into(),
            fs_owner: ".".into(),
            is_dirty: true,
            default_branch: "main".into(),
        };
        checkout_all(tmp.path(), &[task]).await.unwrap();

        let after = git_rev_parse(&clone, "HEAD").await;
        let origin_main = git_rev_parse(&clone, "origin/main").await;
        assert_ne!(before, origin_main, "origin should have advanced after fetch");
        assert_eq!(before, after, "local HEAD must NOT move when staged files block pull");
        assert!(!clone.join("new.txt").exists(), "new file must not appear after fetch-only");
    }

    #[tokio::test]
    async fn fetch_only_when_on_non_default_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let origin = tmp.path().join("origin.git");
        let clone = tmp.path().join("clone");
        setup_origin_and_clone(&origin, &clone).await;

        git_ok(&clone, &["checkout", "-b", "feature"]).await;
        let before = git_rev_parse(&clone, "HEAD").await;
        push_new_commit_to_origin(tmp.path(), &origin, "new.txt", "new").await;

        let task = CheckoutTask {
            owner: "o".into(),
            name: "clone".into(),
            fs_owner: ".".into(),
            is_dirty: true,
            default_branch: "main".into(),
        };
        checkout_all(tmp.path(), &[task]).await.unwrap();

        let after = git_rev_parse(&clone, "HEAD").await;
        let origin_main = git_rev_parse(&clone, "origin/main").await;
        assert_ne!(before, origin_main, "origin should have advanced after fetch");
        assert_eq!(before, after, "local HEAD must NOT move when on a feature branch");
        assert!(!clone.join("new.txt").exists(), "new file must not appear after fetch-only");
    }

    #[tokio::test]
    async fn skip_does_no_git_operations_when_not_dirty() {
        let tmp = tempfile::tempdir().unwrap();
        let origin = tmp.path().join("origin.git");
        let clone = tmp.path().join("clone");
        setup_origin_and_clone(&origin, &clone).await;

        let before = git_rev_parse(&clone, "HEAD").await;
        push_new_commit_to_origin(tmp.path(), &origin, "new.txt", "new").await;

        let task = CheckoutTask {
            owner: "o".into(),
            name: "clone".into(),
            fs_owner: ".".into(),
            is_dirty: false,
            default_branch: "main".into(),
        };
        checkout_all(tmp.path(), &[task]).await.unwrap();

        let after = git_rev_parse(&clone, "HEAD").await;
        let origin_main = git_rev_parse(&clone, "origin/main").await;
        assert_eq!(before, after, "skip must not touch local HEAD");
        assert_eq!(before, origin_main, "skip must not fetch origin");
        assert!(!clone.join("new.txt").exists(), "new file must not appear when skipped");
    }

    #[tokio::test]
    async fn working_tree_clean_when_truly_clean() {
        let tmp = tempfile::tempdir().unwrap();
        let origin = tmp.path().join("origin.git");
        let clone = tmp.path().join("clone");
        setup_origin_and_clone(&origin, &clone).await;

        assert!(git_working_tree_clean(&clone).await.unwrap(), "fresh clone should be clean");
        assert!(should_pull(&clone, "main").await.unwrap(), "clean + on main => should pull");
    }
}
