use anyhow::{anyhow, bail, Context, Result};
use sqlx::SqlitePool;
use std::path::Path;
use std::sync::Arc;
use tokio::process::Command;
use tokio::sync::Semaphore;

/// Default cap on concurrent per-repo git operations during a checkout sweep.
/// Override with `GHCACHE_CHECKOUT_CONCURRENCY`. Without this, a few-hundred-repo
/// sweep spawns a few-hundred simultaneous `git fetch`/`gh clone` subprocesses
/// and hogs the machine.
const DEFAULT_CHECKOUT_CONCURRENCY: usize = 4;

fn checkout_concurrency() -> usize {
    std::env::var("GHCACHE_CHECKOUT_CONCURRENCY")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(DEFAULT_CHECKOUT_CONCURRENCY)
}

/// Cap on git delta-resolution threads per fetch/clone subprocess. `index-pack`
/// otherwise spins up one thread per core, so a single fetch can saturate the
/// machine. Default 1; override with `GHCACHE_GIT_THREADS`.
fn git_pack_threads() -> usize {
    std::env::var("GHCACHE_GIT_THREADS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(1)
}

/// `nice` increment for fetch/clone subprocesses so the sweep yields CPU to
/// whatever the user is actively running. 0 disables the `nice` wrapper.
/// Override with `GHCACHE_GIT_NICE`.
fn git_niceness() -> i32 {
    std::env::var("GHCACHE_GIT_NICE")
        .ok()
        .and_then(|v| v.parse::<i32>().ok())
        .unwrap_or(10)
}

/// Build a fetch/clone command tuned to stay calm under an active workload: delta
/// threads capped via `GIT_CONFIG_*` (which also reaches the `git` that
/// `gh repo clone` forks) and the process run at a lowered scheduling priority via
/// `nice`. Read-only plumbing (status, rev-parse, ...) is cheap and skips this.
fn calm_fetch_command(program: &str, args: &[&str]) -> Command {
    let nice = git_niceness();
    let mut cmd = if nice > 0 {
        let mut c = Command::new("nice");
        c.arg("-n").arg(nice.to_string()).arg(program).args(args);
        c
    } else {
        let mut c = Command::new(program);
        c.args(args);
        c
    };
    cmd.env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", "pack.threads")
        .env("GIT_CONFIG_VALUE_0", git_pack_threads().to_string());
    cmd
}

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
    /// Mirror GitHub pull request heads into refs/remotes/pr/<number>/head.
    pub fetch_pr_branches: bool,
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

/// Watch-driven checkout sweep: clone missing dirs and force-update the default
/// branch. If the default branch is checked out, local changes are stashed before
/// resetting to origin/<default>. If another branch is checked out, that branch is
/// left alone and the local default branch ref is force-updated in the background.
///
/// Decision matrix for an existing clone:
/// - on default branch     -> stash if needed, fetch, reset --hard origin/default
/// - on another branch     -> fetch, branch -f default origin/default
pub async fn checkout_all(
    staging: &Path,
    tasks: &[CheckoutTask],
) -> Result<()> {
    let limit = checkout_concurrency();
    let sem = Arc::new(Semaphore::new(limit));
    tracing::info!(total = tasks.len(), concurrency = limit, "checkout: sweep starting");
    let mut handles: Vec<tokio::task::JoinHandle<(String, Result<()>)>> = vec![];
    for t in tasks {
        let staging = staging.to_path_buf();
        let owner = t.owner.clone();
        let name = t.name.clone();
        let fs_owner = t.fs_owner.clone();
        let default_branch = t.default_branch.clone();
        let fetch_pr_branches = t.fetch_pr_branches;
        let slug = format!("{owner}/{name}");
        let sem = sem.clone();
        handles.push(tokio::spawn(async move {
            // Park here until a permit frees up; caps concurrent git subprocesses
            // at `limit` no matter how many repos are in the sweep.
            let _permit = match sem.acquire_owned().await {
                Ok(p) => p,
                Err(_) => return (slug, Err(anyhow!("checkout semaphore closed"))),
            };
            let local_path = staging.join(&fs_owner).join(&name);
            tracing::info!(
                %slug,
                path = %local_path.display(),
                branch = %default_branch,
                fetch_pr_branches,
                exists = local_path.exists(),
                "checkout: repo start"
            );
            let res = if !local_path.exists() {
                tracing::info!(%slug, path = %local_path.display(), "checkout: cloning");
                run_clone(&slug, &local_path).await
            } else {
                force_update_default_branch(&slug, &local_path, &default_branch).await
            };
            let res = match res {
                Ok(()) if fetch_pr_branches => run_fetch_pr_heads(&local_path).await,
                other => other,
            };
            (slug, res)
        }));
    }
    let mut failed = 0usize;
    for h in handles {
        let (slug, res) = h.await.map_err(|_| anyhow!("checkout task panicked"))?;
        if let Err(e) = res {
            failed += 1;
            tracing::error!(%slug, error = %e, "checkout: repo failed");
        }
    }
    tracing::info!(total = tasks.len(), failed, "checkout: finished");
    Ok(())
}

async fn force_update_default_branch(slug: &str, path: &Path, default_branch: &str) -> Result<()> {
    let current = git_current_branch(path).await?;
    if current != default_branch {
        let local_before = git_rev_parse_opt(path, default_branch).await?;
        tracing::info!(
            %slug,
            path = %path.display(),
            current_branch = %current,
            branch = %default_branch,
            local_before = %short_sha(local_before.as_deref()),
            "checkout: force-updating default branch in background"
        );
        run_fetch_default_branch(path, default_branch).await?;
        let remote_after = git_rev_parse_opt(path, &format!("origin/{default_branch}")).await?;
        run_force_branch(path, default_branch).await?;
        let local_after = git_rev_parse_opt(path, default_branch).await?;
        tracing::info!(
            %slug,
            path = %path.display(),
            branch = %default_branch,
            remote_after = %short_sha(remote_after.as_deref()),
            local_after = %short_sha(local_after.as_deref()),
            "default branch force-updated"
        );
        return Ok(());
    }

    let clean = git_working_tree_clean(path).await?;
    let head_before = git_rev_parse_opt(path, "HEAD").await?;
    if !clean {
        tracing::info!(
            %slug,
            path = %path.display(),
            branch = %default_branch,
            "checkout: stashing before force update"
        );
        run_stash(path, default_branch).await?;
    }

    tracing::info!(
        %slug,
        path = %path.display(),
        branch = %default_branch,
        clean,
        head_before = %short_sha(head_before.as_deref()),
        "checkout: force-updating checked-out default branch"
    );
    run_fetch_default_branch(path, default_branch).await?;
    let remote_after = git_rev_parse_opt(path, &format!("origin/{default_branch}")).await?;
    run_reset(path, default_branch).await?;
    let head_after = git_rev_parse_opt(path, "HEAD").await?;
    tracing::info!(
        %slug,
        path = %path.display(),
        branch = %default_branch,
        remote_after = %short_sha(remote_after.as_deref()),
        head_after = %short_sha(head_after.as_deref()),
        "default branch force-updated"
    );
    Ok(())
}

/// Clean for force-update purposes means no *tracked* modifications or staged
/// changes. Untracked files (`??`) and ignored files (`!!`) are deliberately
/// ignored: `reset --hard` leaves untracked files in place, so there is nothing
/// to stash for them. Only tracked diffs would be clobbered by the reset and
/// thus need stashing first.
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
    let text = String::from_utf8_lossy(&output.stdout);
    let has_tracked_changes = text
        .lines()
        .any(|l| !l.trim().is_empty() && !l.starts_with("??") && !l.starts_with("!!"));
    Ok(!has_tracked_changes)
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

async fn git_rev_parse_opt(path: &Path, rev: &str) -> Result<Option<String>> {
    let output = Command::new("git")
        .args(["-C", &path.to_string_lossy(), "rev-parse", rev])
        .output()
        .await
        .context("git rev-parse")?;
    if !output.status.success() {
        return Ok(None);
    }
    Ok(Some(String::from_utf8_lossy(&output.stdout).trim().to_owned()))
}

async fn git_pr_head_count(path: &Path) -> Result<usize> {
    let output = Command::new("git")
        .args(["-C", &path.to_string_lossy(), "for-each-ref", "--format=%(refname)", "refs/remotes/pr"])
        .output()
        .await
        .context("git for-each-ref refs/remotes/pr")?;
    if !output.status.success() {
        bail!(
            "git for-each-ref refs/remotes/pr failed in {}: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| line.ends_with("/head"))
        .count())
}

fn short_sha(sha: Option<&str>) -> &str {
    sha.and_then(|s| s.get(..7)).unwrap_or("missing")
}

async fn run_clone(slug: &str, dest: &Path) -> Result<()> {
    if let Some(parent) = dest.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let status = calm_fetch_command("gh", &["repo", "clone", slug, &dest.to_string_lossy()])
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
    let status = calm_fetch_command("git", &["-C", &path_str, "fetch", "origin"])
        .status()
        .await
        .context("git fetch")?;
    if !status.success() {
        bail!("git fetch failed in {}", path.display());
    }
    tracing::info!(path = %path.display(), "fetched origin");
    Ok(())
}

async fn run_fetch_default_branch(path: &Path, branch: &str) -> Result<()> {
    let path_str = path.to_string_lossy();
    let refspec = format!("+refs/heads/{branch}:refs/remotes/origin/{branch}");
    let status = calm_fetch_command("git", &["-C", &path_str, "fetch", "origin", &refspec])
        .status()
        .await
        .context("git fetch default branch")?;
    if !status.success() {
        bail!("git fetch origin {refspec} failed in {}", path.display());
    }
    tracing::info!(path = %path.display(), %branch, "fetched default branch");
    Ok(())
}

async fn run_fetch_pr_heads(path: &Path) -> Result<()> {
    let path_str = path.to_string_lossy();
    let before = git_pr_head_count(path).await?;
    let status = calm_fetch_command("git", &["-C", &path_str, "fetch", "--prune", "origin", "+refs/pull/*/head:refs/remotes/pr/*/head"])
        .status()
        .await
        .context("git fetch pull request heads")?;
    if !status.success() {
        bail!("git fetch pull request heads failed in {}", path.display());
    }
    let after = git_pr_head_count(path).await?;
    let gone = before.saturating_sub(after);
    tracing::info!(
        path = %path.display(),
        pr_heads_before = before,
        pr_heads_after = after,
        pr_heads_gone = gone,
        pr_heads_present = after,
        "fetched pull request heads"
    );
    Ok(())
}

async fn run_stash(path: &Path, branch: &str) -> Result<()> {
    let path_str = path.to_string_lossy();
    let message = format!("ghcache auto-stash before force-updating {branch}");
    // No --include-untracked: untracked files survive a force update untouched.
    // Only tracked modifications/staged changes (which reset --hard would clobber)
    // are stashed here.
    let status = Command::new("git")
        .args(["-C", &path_str, "stash", "push", "-m", &message])
        .status()
        .await
        .context("git stash push")?;
    if !status.success() {
        bail!("git stash push failed in {}", path.display());
    }
    tracing::info!(path = %path.display(), %branch, "stashed worktree");
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

async fn run_force_branch(path: &Path, branch: &str) -> Result<()> {
    let path_str = path.to_string_lossy();
    let remote_ref = format!("origin/{branch}");
    let status = Command::new("git")
        .args(["-C", &path_str, "branch", "-f", branch, &remote_ref])
        .status()
        .await
        .context("git branch -f")?;
    if !status.success() {
        bail!("git branch -f {branch} {remote_ref} failed in {}", path.display());
    }
    tracing::info!(path = %path.display(), %branch, "branch force-updated to remote");
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

    async fn git_rev_parse_opt(path: &Path, rev: &str) -> Option<String> {
        let out = git_cmd(path, &["rev-parse", rev]).await;
        out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    async fn git_stash_list(path: &Path) -> String {
        let out = git_cmd(path, &["stash", "list"]).await;
        assert!(out.status.success(), "git stash list failed");
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
    async fn force_update_fast_forwards_when_clean_on_default_branch() {
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
            fetch_pr_branches: false,
        };
        checkout_all(tmp.path(), &[task]).await.unwrap();

        let after = git_rev_parse(&clone, "HEAD").await;
        let origin_main = git_rev_parse(&clone, "origin/main").await;
        assert_eq!(after, origin_main, "local main should fast-forward to origin");
        assert_ne!(before, after, "HEAD should have moved after force update");
        assert!(clone.join("new.txt").exists(), "new file should be present after fast-forward");
    }

    #[tokio::test]
    async fn untracked_files_survive_force_update_without_stashing() {
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
            fetch_pr_branches: false,
        };
        checkout_all(tmp.path(), &[task]).await.unwrap();

        let after = git_rev_parse(&clone, "HEAD").await;
        let origin_main = git_rev_parse(&clone, "origin/main").await;
        assert_ne!(before, after, "local HEAD must move after force update");
        assert_eq!(after, origin_main, "local HEAD must reset to origin/main");
        assert!(clone.join("new.txt").exists(), "new file must appear after force update");
        // Untracked files are left in place: reset --hard does not touch them and
        // they must not be swept into a stash.
        assert!(clone.join("untracked.txt").exists(), "untracked file must survive the force update");
        assert_eq!(git_stash_list(&clone).await, "", "untracked-only dirt must not create a stash");
    }

    #[tokio::test]
    async fn stash_and_force_update_when_modified_files_present_on_default_branch() {
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
            fetch_pr_branches: false,
        };
        checkout_all(tmp.path(), &[task]).await.unwrap();

        let after = git_rev_parse(&clone, "HEAD").await;
        let origin_main = git_rev_parse(&clone, "origin/main").await;
        assert_ne!(before, after, "local HEAD must move after force update");
        assert_eq!(after, origin_main, "local HEAD must reset to origin/main");
        assert!(clone.join("new.txt").exists(), "new file must appear after force update");
        assert!(git_stash_list(&clone).await.contains("ghcache auto-stash"));
    }

    #[tokio::test]
    async fn stash_and_force_update_when_staged_files_present_on_default_branch() {
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
            fetch_pr_branches: false,
        };
        checkout_all(tmp.path(), &[task]).await.unwrap();

        let after = git_rev_parse(&clone, "HEAD").await;
        let origin_main = git_rev_parse(&clone, "origin/main").await;
        assert_ne!(before, after, "local HEAD must move after force update");
        assert_eq!(after, origin_main, "local HEAD must reset to origin/main");
        assert!(clone.join("new.txt").exists(), "new file must appear after force update");
        assert!(git_stash_list(&clone).await.contains("ghcache auto-stash"));
    }

    #[tokio::test]
    async fn force_update_default_branch_without_switching_feature_branch() {
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
            fetch_pr_branches: false,
        };
        checkout_all(tmp.path(), &[task]).await.unwrap();

        let after = git_rev_parse(&clone, "HEAD").await;
        let origin_main = git_rev_parse(&clone, "origin/main").await;
        let local_main = git_rev_parse(&clone, "main").await;
        assert_ne!(before, origin_main, "origin should have advanced after fetch");
        assert_eq!(before, after, "local HEAD must not move when on a feature branch");
        assert_eq!(local_main, origin_main, "local main must update in the background");
        assert!(!clone.join("new.txt").exists(), "feature worktree must not receive main files");
    }

    #[tokio::test]
    async fn force_update_handles_non_fast_forward_default_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let origin = tmp.path().join("origin.git");
        let clone = tmp.path().join("clone");
        setup_origin_and_clone(&origin, &clone).await;

        tokio::fs::write(clone.join("local.txt"), "local").await.unwrap();
        git_ok(&clone, &["add", "local.txt"]).await;
        git_ok(&clone, &["commit", "-m", "local"]).await;
        let local_commit = git_rev_parse(&clone, "HEAD").await;

        push_new_commit_to_origin(tmp.path(), &origin, "remote.txt", "remote").await;

        let task = CheckoutTask {
            owner: "o".into(),
            name: "clone".into(),
            fs_owner: ".".into(),
            is_dirty: false,
            default_branch: "main".into(),
            fetch_pr_branches: false,
        };
        checkout_all(tmp.path(), &[task]).await.unwrap();

        let after = git_rev_parse(&clone, "HEAD").await;
        let origin_main = git_rev_parse(&clone, "origin/main").await;
        assert_ne!(local_commit, after, "local divergent commit must not remain checked out");
        assert_eq!(after, origin_main, "local main must force-reset to origin/main");
        assert!(clone.join("remote.txt").exists(), "remote file must appear after force update");
        assert!(!clone.join("local.txt").exists(), "local divergent file must leave the worktree");
    }

    #[tokio::test]
    async fn force_update_happens_even_when_not_dirty() {
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
            fetch_pr_branches: false,
        };
        checkout_all(tmp.path(), &[task]).await.unwrap();

        let after = git_rev_parse(&clone, "HEAD").await;
        let origin_main = git_rev_parse(&clone, "origin/main").await;
        assert_ne!(before, after, "local HEAD must advance even when not dirty");
        assert_eq!(after, origin_main, "force update must fast-forward to origin/main");
        assert!(clone.join("new.txt").exists(), "new file must appear after force update");
    }

    #[tokio::test]
    async fn force_update_always_happens_even_when_not_dirty() {
        let tmp = tempfile::tempdir().unwrap();
        let origin = tmp.path().join("origin.git");
        let clone = tmp.path().join("o").join("n");
        tokio::fs::create_dir_all(clone.parent().unwrap()).await.unwrap();
        setup_origin_and_clone(&origin, &clone).await;

        let before = git_rev_parse(&clone, "origin/main").await;
        push_new_commit_to_origin(tmp.path(), &origin, "new.txt", "new").await;

        let task = CheckoutTask {
            owner: "o".into(),
            name: "n".into(),
            fs_owner: "o".into(),
            is_dirty: false,
            default_branch: "main".into(),
            fetch_pr_branches: false,
        };
        checkout_all(tmp.path(), &[task]).await.unwrap();

        let after = git_rev_parse(&clone, "origin/main").await;
        assert_ne!(before, after, "origin/main must advance even when not dirty");
        assert_eq!(git_rev_parse(&clone, "HEAD").await, after, "HEAD must advance when clean on default branch");
    }

    #[tokio::test]
    async fn fetches_pull_request_heads_into_pr_remote_namespace() {
        let tmp = tempfile::tempdir().unwrap();
        let origin = tmp.path().join("origin.git");
        let clone = tmp.path().join("o").join("n");
        tokio::fs::create_dir_all(clone.parent().unwrap()).await.unwrap();
        setup_origin_and_clone(&origin, &clone).await;
        push_new_commit_to_origin(tmp.path(), &origin, "pr.txt", "pr").await;
        let pr_sha = git_rev_parse(&origin, "refs/heads/main").await;
        git_ok(&origin, &["update-ref", "refs/pull/17/head", &pr_sha]).await;

        let task = CheckoutTask {
            owner: "o".into(),
            name: "n".into(),
            fs_owner: "o".into(),
            is_dirty: false,
            default_branch: "main".into(),
            fetch_pr_branches: true,
        };
        checkout_all(tmp.path(), &[task]).await.unwrap();

        let fetched = git_rev_parse(&clone, "refs/remotes/pr/17/head").await;
        assert_eq!(fetched, pr_sha);
        assert_eq!(git_pr_head_count(&clone).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn prunes_deleted_pull_request_heads() {
        let tmp = tempfile::tempdir().unwrap();
        let origin = tmp.path().join("origin.git");
        let clone = tmp.path().join("o").join("n");
        tokio::fs::create_dir_all(clone.parent().unwrap()).await.unwrap();
        setup_origin_and_clone(&origin, &clone).await;
        let pr_sha = git_rev_parse(&origin, "refs/heads/main").await;
        git_ok(&origin, &["update-ref", "refs/pull/17/head", &pr_sha]).await;

        let task = CheckoutTask {
            owner: "o".into(),
            name: "n".into(),
            fs_owner: "o".into(),
            is_dirty: false,
            default_branch: "main".into(),
            fetch_pr_branches: true,
        };
        checkout_all(tmp.path(), &[task]).await.unwrap();
        assert!(git_rev_parse_opt(&clone, "refs/remotes/pr/17/head").await.is_some());

        git_ok(&origin, &["update-ref", "-d", "refs/pull/17/head"]).await;
        let task = CheckoutTask {
            owner: "o".into(),
            name: "n".into(),
            fs_owner: "o".into(),
            is_dirty: false,
            default_branch: "main".into(),
            fetch_pr_branches: true,
        };
        checkout_all(tmp.path(), &[task]).await.unwrap();

        assert!(git_rev_parse_opt(&clone, "refs/remotes/pr/17/head").await.is_none());
        assert_eq!(git_pr_head_count(&clone).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn working_tree_clean_when_truly_clean() {
        let tmp = tempfile::tempdir().unwrap();
        let origin = tmp.path().join("origin.git");
        let clone = tmp.path().join("clone");
        setup_origin_and_clone(&origin, &clone).await;

        assert!(git_working_tree_clean(&clone).await.unwrap(), "fresh clone should be clean");
    }

    #[test]
    fn checkout_concurrency_defaults_and_parses_override() {
        // Default when unset/invalid.
        std::env::remove_var("GHCACHE_CHECKOUT_CONCURRENCY");
        assert_eq!(checkout_concurrency(), DEFAULT_CHECKOUT_CONCURRENCY);

        std::env::set_var("GHCACHE_CHECKOUT_CONCURRENCY", "0");
        assert_eq!(checkout_concurrency(), DEFAULT_CHECKOUT_CONCURRENCY, "0 is rejected, falls back to default");

        std::env::set_var("GHCACHE_CHECKOUT_CONCURRENCY", "nonsense");
        assert_eq!(checkout_concurrency(), DEFAULT_CHECKOUT_CONCURRENCY, "garbage falls back to default");

        std::env::set_var("GHCACHE_CHECKOUT_CONCURRENCY", "3");
        assert_eq!(checkout_concurrency(), 3);

        std::env::remove_var("GHCACHE_CHECKOUT_CONCURRENCY");
    }

    /// A pool of tasks gated by `Semaphore::new(2)` must never have more than 2
    /// running at once. This is the exact mechanism `checkout_all` uses to cap
    /// concurrent git subprocesses; the env wiring is covered by the parse test.
    #[tokio::test]
    async fn semaphore_caps_concurrent_tasks() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let sem = Arc::new(Semaphore::new(2));
        let peak = Arc::new(AtomicUsize::new(0));
        let live = Arc::new(AtomicUsize::new(0));
        let mut handles = vec![];
        for _ in 0..16 {
            let sem = sem.clone();
            let peak = peak.clone();
            let live = live.clone();
            handles.push(tokio::spawn(async move {
                let _p = sem.acquire_owned().await.unwrap();
                let now = live.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now, Ordering::SeqCst);
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                live.fetch_sub(1, Ordering::SeqCst);
            }));
        }
        for h in handles { h.await.unwrap(); }
        assert!(
            peak.load(Ordering::SeqCst) <= 2,
            "peak concurrency {} exceeded cap 2",
            peak.load(Ordering::SeqCst)
        );
    }

    /// A real sweep over several already-cloned repos completes successfully with
    /// the cap in force (the default cap, no env override needed).
    #[tokio::test]
    async fn capped_sweep_updates_all_repos() {
        let tmp = tempfile::tempdir().unwrap();
        let origin = tmp.path().join("origin.git");
        let seed = tmp.path().join("seed");
        setup_origin_and_clone(&origin, &seed).await;
        push_new_commit_to_origin(tmp.path(), &origin, "new.txt", "new").await;

        let mut tasks = vec![];
        for i in 0..6 {
            let fs_owner = format!("o{i}");
            let dest = tmp.path().join(&fs_owner).join("n");
            tokio::fs::create_dir_all(dest.parent().unwrap()).await.unwrap();
            git_ok(tmp.path(), &["clone", origin.to_str().unwrap(), dest.to_str().unwrap()]).await;
            tasks.push(CheckoutTask {
                owner: "o".into(),
                name: "n".into(),
                fs_owner,
                is_dirty: false,
                default_branch: "main".into(),
                fetch_pr_branches: false,
            });
        }

        checkout_all(tmp.path(), &tasks).await.unwrap();

        for i in 0..6 {
            let dest = tmp.path().join(format!("o{i}")).join("n");
            assert!(dest.join("new.txt").exists(), "repo o{i} must be updated to origin/main");
        }
    }
}
