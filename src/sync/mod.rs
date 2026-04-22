pub mod branches;
pub mod events;
pub mod notifications;
pub mod prs;

use anyhow::Result;
use sqlx::SqlitePool;
use std::collections::{HashMap, HashSet};
use std::time::Duration;

use crate::checkout;
use crate::config::{OrgConfig, RepoConfig, ResolvedConfig};
use crate::db;
use crate::gh::{self, GhRequest, GitHubClient};

pub struct SyncFilter {
    pub repo:        Option<String>,
    pub prs_only:    bool,
    pub notifs_only: bool,
    pub events_only: bool,
}

impl SyncFilter {
    fn all(&self) -> bool {
        !self.prs_only && !self.notifs_only && !self.events_only
    }

    fn do_prs(&self) -> bool {
        self.all() || self.prs_only
    }

    fn do_notifs(&self) -> bool {
        self.all() || self.notifs_only
    }

    fn do_events(&self) -> bool {
        self.all() || self.events_only
    }
}

const NOT_ORG: &str = "__user__";

async fn discover_org_repos(
    conn: &mut sqlx::SqliteConnection,
    gh: &dyn GitHubClient,
    owner: &str,
) -> Result<Vec<String>> {
    let org_endpoint  = format!("/orgs/{owner}/repos?per_page=100");
    let user_endpoint = format!("/users/{owner}/repos?per_page=100");

    let poll = db::get_poll_state(conn, &org_endpoint).await?;

    // Repo lists change rarely. Skip the API call if polled in the last 24 hours.
    if let Some(ref ts) = poll.last_polled_at {
        if let Ok(last) = chrono::DateTime::parse_from_rfc3339(ts) {
            if chrono::Utc::now().signed_duration_since(last) < chrono::Duration::hours(24) {
                tracing::debug!(owner, "org repo list polled <24h ago, using DB cache");
                return repos_from_db(conn, owner).await;
            }
        }
    }

    let body = if poll.etag.as_deref() == Some(NOT_ORG) {
        let poll2 = db::get_poll_state(conn, &user_endpoint).await?;
        let mut req2 = GhRequest::get(&user_endpoint).paginated();
        if let Some(ref etag) = poll2.etag { req2 = req2.with_etag(etag); }
        let resp2 = gh.call(conn, &req2).await?;
        if resp2.is_not_modified() { return repos_from_db(conn, owner).await; }
        resp2.body
    } else {
        let mut req = GhRequest::get(&org_endpoint).paginated();
        if let Some(ref etag) = poll.etag { req = req.with_etag(etag); }
        let resp = gh.call(conn, &req).await?;

        if resp.status == 404 {
            db::set_poll_state(conn, &org_endpoint, Some(NOT_ORG), None, None, false).await?;
            let poll2 = db::get_poll_state(conn, &user_endpoint).await?;
            let mut req2 = GhRequest::get(&user_endpoint).paginated();
            if let Some(ref etag) = poll2.etag { req2 = req2.with_etag(etag); }
            let resp2 = gh.call(conn, &req2).await?;
            if resp2.is_not_modified() { return repos_from_db(conn, owner).await; }
            resp2.body
        } else if resp.is_not_modified() {
            return repos_from_db(conn, owner).await;
        } else {
            resp.body
        }
    };

    let names: Vec<String> = body
        .as_array()
        .map(|arr| arr.iter().filter_map(|r| r["name"].as_str().map(str::to_owned)).collect())
        .unwrap_or_default();

    tracing::info!(owner, count = names.len() as u64, "discovered org repos");
    Ok(names)
}

async fn repos_from_db(conn: &mut sqlx::SqliteConnection, owner: &str) -> Result<Vec<String>> {
    let names: Vec<String> = sqlx::query_scalar("SELECT name FROM repo WHERE owner = ? ORDER BY name")
        .bind(owner)
        .fetch_all(conn)
        .await?;
    Ok(names)
}

fn org_to_repos(org: &OrgConfig, names: Vec<String>) -> Vec<RepoConfig> {
    names
        .into_iter()
        .filter(|n| !org.exclude.contains(n))
        .map(|name| RepoConfig {
            owner:                org.owner.clone(),
            name,
            default_branch:       None,
            sync_prs:             org.sync_prs,
            sync_notifications:   org.sync_notifications,
            sync_events:          org.sync_events,
            sync_branches:        org.sync_branches.clone(),
            checkout_on_sync:     org.checkout_on_sync,
            checkout_pr_branches: org.checkout_pr_branches,
            fs_alias:             org.fs_alias.clone(),
        })
        .collect()
}

pub async fn run(
    pool: &SqlitePool,
    gh: &dyn GitHubClient,
    cfg: &ResolvedConfig,
    filter: SyncFilter,
    full_sweep: bool,
    extra_repos: &[RepoConfig],
    extra_notif_slugs: &[(String, String)],
    gh_username: &str,
) -> Result<HashSet<(String, String)>> {
    let mut dirty_repos: HashSet<(String, String)> = HashSet::new();
    let mut org_repos: Vec<RepoConfig> = vec![];
    let mut org_owners: HashSet<String> = HashSet::new();
    {
        let mut conn = pool.acquire().await?;
        for org in &cfg.orgs {
            match discover_org_repos(&mut *conn, gh, &org.owner).await {
                Ok(names) => {
                    org_owners.insert(org.owner.clone());
                    org_repos.extend(org_to_repos(org, names));
                }
                Err(e) => tracing::error!(owner = %org.owner, error = %e, "org repo discovery failed"),
            }
        }
    }

    let mut org_dirty: HashMap<(String, String), Vec<i64>> = HashMap::new();
    if filter.do_events() {
        let mut conn = pool.acquire().await?;
        for org in &cfg.orgs {
            if org.sync_events.unwrap_or(false) {
                match events::sync_org(&mut *conn, gh, &org.owner, gh_username).await {
                    Ok(dirty) => {
                        for (name, prs) in dirty {
                            org_dirty.insert((org.owner.clone(), name), prs);
                        }
                    }
                    Err(e) => tracing::error!(owner = %org.owner, error = %e, "org events sync failed"),
                }
            }
        }
    }

    let all_repos: Vec<&RepoConfig> = cfg.repos.iter()
        .chain(org_repos.iter())
        .chain(extra_repos.iter())
        .collect();

    // Collect repos needing a full-sweep PR fetch so we can batch them.
    let mut pr_batch: Vec<(i64, String, String)> = vec![];

    for repo in &all_repos {
        if let Some(ref slug) = filter.repo {
            if &repo.slug() != slug { continue; }
        }

        // For org repos in targeted mode, skip entirely if no events this pass.
        let is_org_repo = org_owners.contains(&repo.owner);
        let org_has_activity = org_dirty.contains_key(&(repo.owner.clone(), repo.name.clone()));
        if !full_sweep && is_org_repo && !org_has_activity && filter.all() {
            continue;
        }

        tracing::info!(repo = %repo.slug(), full_sweep, "syncing");

        // Throttle outside the per-repo transaction.
        {
            let mut c = pool.acquire().await?;
            gh.throttle_if_needed(&mut *c, "rest").await?;
        }

        let mut tx = pool.begin().await?;
        let mut dirty_prs: Vec<i64> = vec![];
        let mut had_activity = false;
        let mut needs_pr_batch = false;

        let result: Result<()> = async {
            let default_branch = repo.default_branch.as_deref().unwrap_or("main");
            let repo_id = db::upsert_repo(&mut *tx, &repo.owner, &repo.name, default_branch).await?;

            if filter.do_events() && repo.sync_events.unwrap_or(false) {
                if is_org_repo {
                    had_activity = org_dirty.contains_key(&(repo.owner.clone(), repo.name.clone()));
                    dirty_prs = org_dirty
                        .get(&(repo.owner.clone(), repo.name.clone()))
                        .cloned()
                        .unwrap_or_default();
                } else {
                    let res = events::sync(&mut *tx, gh, repo_id, &repo.owner, &repo.name).await?;
                    had_activity = res.had_activity;
                    dirty_prs = res.dirty_prs;
                }
            }

            if filter.do_prs() && repo.sync_prs.unwrap_or(false) {
                if full_sweep {
                    needs_pr_batch = true;
                } else {
                    prs::sync_targeted(&mut *tx, gh, repo_id, &repo.owner, &repo.name, &dirty_prs).await?;
                }
            }

            if repo.sync_branches.as_ref().map(|b| !b.is_empty()).unwrap_or(false) && filter.all() {
                branches::sync(
                    &mut *tx,
                    gh,
                    repo_id,
                    &repo.owner,
                    &repo.name,
                    repo.sync_branches.as_deref().unwrap_or(&[]),
                )
                .await?;
            }

            Ok(())
        }
        .await;

        if let Err(e) = result {
            tracing::error!(repo = %repo.slug(), error = %e, "sync failed, rolling back");
            let _ = tx.rollback().await;
        } else {
            tx.commit().await?;
            if had_activity {
                dirty_repos.insert((repo.owner.clone(), repo.name.clone()));
            }
            // Only add to batch after successful commit so repo_id is durable.
            if needs_pr_batch {
                let mut c = pool.acquire().await?;
                if let Ok(Some(repo_id)) = db::get_repo_id(&mut *c, &repo.owner, &repo.name).await {
                    pr_batch.push((repo_id, repo.owner.clone(), repo.name.clone()));
                }
            }
        }
    }

    // Batched full-sweep PR sync: packs multiple repos per GraphQL call.
    if !pr_batch.is_empty() {
        let mut conn = pool.acquire().await?;
        prs::sync_batch(&mut *conn, gh, &pr_batch).await?;
    }

    let notifs_needed = filter.do_notifs()
        && (cfg.sync_notifications
            || cfg.repos.iter().any(|r| r.sync_notifications.unwrap_or(false))
            || !extra_notif_slugs.is_empty());
    if notifs_needed {
        let mut conn = pool.acquire().await?;
        notifications::sync(&mut *conn, gh, cfg, extra_notif_slugs).await?;
    }

    Ok(dirty_repos)
}

/// Build one CheckoutTask per repo with a checkout_* flag set. `is_dirty` reflects
/// whether the repo appears in `dirty_repos` this cycle. Skipping and deduplication
/// is the invariant: N events on repo R produce exactly one task with is_dirty=true.
pub async fn build_checkout_tasks(
    conn: &mut sqlx::SqliteConnection,
    cfg: &ResolvedConfig,
    dirty_repos: &HashSet<(String, String)>,
) -> Vec<checkout::CheckoutTask> {
    let mut tasks: Vec<checkout::CheckoutTask> = vec![];

    for r in &cfg.repos {
        let wants_on_sync = r.checkout_on_sync.unwrap_or(false);
        let wants_pr_fetch = r.checkout_pr_branches.unwrap_or(false);
        if !wants_on_sync && !wants_pr_fetch { continue; }
        let default_branch = match db::get_repo_default_branch(&mut *conn, &r.owner, &r.name).await {
            Ok(Some(b)) => b,
            _ => continue,
        };
        let is_dirty = dirty_repos.contains(&(r.owner.clone(), r.name.clone()));
        tasks.push(checkout::CheckoutTask {
            owner: r.owner.clone(),
            name: r.name.clone(),
            fs_owner: r.fs_owner().to_owned(),
            is_dirty,
            default_branch,
        });
    }

    for org in &cfg.orgs {
        let wants_on_sync = org.checkout_on_sync.unwrap_or(false);
        let wants_pr_fetch = org.checkout_pr_branches.unwrap_or(false);
        if !wants_on_sync && !wants_pr_fetch { continue; }
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT name, default_branch FROM repo WHERE owner = ?",
        )
        .bind(&org.owner)
        .fetch_all(&mut *conn)
        .await
        .unwrap_or_default();
        for (name, default_branch) in rows {
            let is_dirty = dirty_repos.contains(&(org.owner.clone(), name.clone()));
            tasks.push(checkout::CheckoutTask {
                owner: org.owner.clone(),
                name,
                fs_owner: org.fs_owner().to_owned(),
                is_dirty,
                default_branch,
            });
        }
    }

    tasks
}

pub async fn watch(
    pool: &SqlitePool,
    gh: &dyn GitHubClient,
    cfg: &ResolvedConfig,
    subs: Option<std::sync::Arc<crate::cmd::Subscriptions>>,
    paused: std::sync::Arc<std::sync::atomic::AtomicBool>,
    force_full_sweep: bool,
) -> Result<()> {
    tracing::info!("starting watch loop");
    let gh_username = gh::authenticated_username(&cfg.gh_binary).await?;
    let mut first_run = if force_full_sweep {
        true
    } else {
        // Skip full sweep on startup if we have cached PR data.
        let has_data: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM pull_request LIMIT 1)",
        )
        .fetch_one(pool)
        .await
        .unwrap_or(false);
        if has_data {
            tracing::info!("skipping full sweep: PR data is cached and up to date (use --full-sweep to force)");
            false
        } else {
            true
        }
    };
    loop {
        if paused.load(std::sync::atomic::Ordering::Relaxed) {
            tracing::debug!("sync paused");
            tokio::time::sleep(Duration::from_millis(500)).await;
            continue;
        }

        let filter = SyncFilter {
            repo: None, prs_only: false, notifs_only: false, events_only: false,
        };

        let (extra_repos, extra_notif_slugs): (Vec<RepoConfig>, Vec<(String, String)>) = subs
            .as_ref()
            .map(|s| {
                let pr_repos    = s.active_pr_sync_repos();
                let notif_repos = s.active_notifications_repos();

                let extra_repos = pr_repos
                    .into_iter()
                    .filter(|(o, n)| !cfg.repos.iter().any(|r| &r.owner == o && &r.name == n))
                    .map(|(owner, name)| RepoConfig {
                        owner,
                        name,
                        default_branch:        None,
                        sync_prs:              Some(true),
                        sync_notifications:    None,
                        sync_events:           None,
                        sync_branches:         None,
                        checkout_on_sync:      None,
                        checkout_pr_branches:  None,
                        fs_alias:              None,
                    })
                    .collect();

                let extra_notif_slugs = notif_repos
                    .into_iter()
                    .filter(|(o, n)| !cfg.repos.iter().any(|r| &r.owner == o && &r.name == n))
                    .collect();

                (extra_repos, extra_notif_slugs)
            })
            .unwrap_or_default();

        if !extra_repos.is_empty() {
            tracing::debug!(count = extra_repos.len(), "syncing subscription repos");
        }

        let dirty_repos = match run(pool, gh, cfg, filter, first_run, &extra_repos, &extra_notif_slugs, &gh_username).await {
            Ok(d) => d,
            Err(e) => {
                tracing::error!(error = %e, "watch sync error");
                HashSet::new()
            }
        };
        first_run = false;

        // Checkout tasks: one per repo, gated by in-memory dirty signal.
        {
            let mut conn = pool.acquire().await?;
            let tasks = build_checkout_tasks(&mut *conn, cfg, &dirty_repos).await;
            if !tasks.is_empty() {
                if let Err(e) = checkout::checkout_all(&cfg.staging_folder, &tasks).await {
                    tracing::error!(error = %e, "checkout_all failed");
                }
            }
        }

        let interval = cfg.poll_interval_seconds;
        tracing::debug!(sleep_seconds = interval, "watch sleeping");
        if let Some(s) = &subs {
            if s.wait_for_sync(Duration::from_secs(interval)).await {
                tracing::debug!("watch woken by new subscription");
            }
        } else {
            tokio::time::sleep(Duration::from_secs(interval)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RepoConfig;
    use crate::db;
    use crate::gh::MockGhClient;

    fn cfg_with_repo(owner: &str, name: &str) -> ResolvedConfig {
        ResolvedConfig {
            db_path:               std::path::PathBuf::from(":memory:"),
            staging_folder:        std::path::PathBuf::from("/tmp"),
            poll_interval_seconds: 60,
            log_level:             "info".into(),
            gh_binary:             "gh".into(),
            rate_warn_threshold:   500,
            rate_stop_threshold:   50,
            cmd_port:              7748,
            heartbeat_ttl_seconds: 30,
            sync_notifications:    false,
            repos: vec![RepoConfig {
                owner:                owner.into(),
                name:                 name.into(),
                default_branch:       Some("main".into()),
                sync_prs:             Some(true),
                sync_events:          Some(true),
                sync_notifications:   Some(false),
                sync_branches:        None,
                checkout_on_sync:     None,
                checkout_pr_branches: None,
                fs_alias:             None,
            }],
            orgs: vec![],
            owner_fs_aliases: std::collections::HashMap::new(),
        }
    }

    fn all_filter() -> SyncFilter {
        SyncFilter { repo: None, prs_only: false, notifs_only: false, events_only: false }
    }

#[tokio::test]
    async fn full_sweep_calls_graphql_once() {
        let pool = db::open_in_memory().await.unwrap();
        let mock = MockGhClient::new();
        mock.push_rest(serde_json::json!([])); // events
        mock.push_graphql(serde_json::json!({"repo_0": {"pullRequests": {"nodes": []}}}));

        run(&pool, &mock, &cfg_with_repo("o", "n"), all_filter(), true, &[], &[], "testuser").await.unwrap();

        assert_eq!(mock.graphql_call_count(), 1);
        let (endpoint, _) = &mock.graphql_calls.lock().unwrap()[0];
        assert!(endpoint.starts_with("graphql:prs:batch/"), "expected batch endpoint, got {endpoint}");
    }

    #[tokio::test]
    async fn full_sweep_batches_multiple_repos() {
        let pool = db::open_in_memory().await.unwrap();
        let mock = MockGhClient::new();
        // 2 repos: each gets an events REST call (304), then 1 batched GraphQL call
        mock.push_rest_304(); // events repo A
        mock.push_rest_304(); // events repo B
        // Single batch response covering both repos
        mock.push_graphql(serde_json::json!({
            "repo_0": {"pullRequests": {"nodes": []}},
            "repo_1": {"pullRequests": {"nodes": []}}
        }));

        let mut cfg = cfg_with_repo("o", "a");
        cfg.repos.push(RepoConfig {
            owner: "o".into(),
            name: "b".into(),
            default_branch: Some("main".into()),
            sync_prs: Some(true),
            sync_events: Some(true),
            sync_notifications: Some(false),
            sync_branches: None,
            checkout_on_sync: None,
            checkout_pr_branches: None,
            fs_alias: None,
        });

        run(&pool, &mock, &cfg, all_filter(), true, &[], &[], "testuser").await.unwrap();

        // Both repos should be packed into a single GraphQL call
        assert_eq!(mock.graphql_call_count(), 1, "expected 1 batched GraphQL call, got {}", mock.graphql_call_count());
    }

    #[tokio::test]
    async fn targeted_no_events_skips_graphql() {
        let pool = db::open_in_memory().await.unwrap();
        let mock = MockGhClient::new();
        mock.push_rest_304();

        run(&pool, &mock, &cfg_with_repo("o", "n"), all_filter(), false, &[], &[], "testuser").await.unwrap();

        assert_eq!(mock.graphql_call_count(), 0);
    }

    #[tokio::test]
    async fn targeted_batches_multiple_prs_in_one_call() {
        let pool = db::open_in_memory().await.unwrap();
        let mock = MockGhClient::new();
        mock.push_rest(serde_json::json!([
            {"id": "1", "type": "PullRequestEvent", "actor": {"login": "alice"},
             "payload": {"number": 42, "action": "opened"}, "created_at": "2026-01-01T00:00:00Z"},
            {"id": "2", "type": "PullRequestReviewEvent", "actor": {"login": "bob"},
             "payload": {"action": "submitted", "pull_request": {"number": 17}},
             "created_at": "2026-01-01T00:00:01Z"},
        ]));
        mock.push_graphql(serde_json::json!({"repository": {}}));

        run(&pool, &mock, &cfg_with_repo("o", "n"), all_filter(), false, &[], &[], "testuser").await.unwrap();

        assert_eq!(mock.graphql_call_count(), 1, "expected exactly 1 batched graphql call");
        let (endpoint, query) = &mock.graphql_calls.lock().unwrap()[0];
        assert!(endpoint.contains("targeted"), "expected targeted endpoint, got {endpoint}");
        assert!(query.contains("pr_42"), "expected alias pr_42 in query");
        assert!(query.contains("pr_17"), "expected alias pr_17 in query");
    }

    #[tokio::test]
    async fn n_events_produce_one_dirty_checkout_task() {
        // Invariant under test: no matter how many events land for a repo this cycle,
        // the watch loop emits exactly one CheckoutTask with is_dirty=true for it.
        let pool = db::open_in_memory().await.unwrap();
        let mock = MockGhClient::new();
        mock.push_rest(serde_json::json!([
            {"id": "1", "type": "PushEvent", "actor": {"login": "a"},
             "payload": {"ref": "refs/heads/main"}, "created_at": "2026-04-17T00:00:00Z"},
            {"id": "2", "type": "PushEvent", "actor": {"login": "b"},
             "payload": {"ref": "refs/heads/feature-x"}, "created_at": "2026-04-17T00:00:01Z"},
            {"id": "3", "type": "PullRequestEvent", "actor": {"login": "c"},
             "payload": {"number": 42, "action": "opened"}, "created_at": "2026-04-17T00:00:02Z"},
            {"id": "4", "type": "PullRequestEvent", "actor": {"login": "d"},
             "payload": {"number": 17, "action": "synchronize"}, "created_at": "2026-04-17T00:00:03Z"},
            {"id": "5", "type": "PushEvent", "actor": {"login": "e"},
             "payload": {"ref": "refs/heads/main"}, "created_at": "2026-04-17T00:00:04Z"},
        ]));
        mock.push_graphql(serde_json::json!({"repo_0": {"pullRequests": {"nodes": []}}}));

        let mut cfg = cfg_with_repo("o", "n");
        cfg.repos[0].checkout_pr_branches = Some(true);

        let dirty = run(&pool, &mock, &cfg, all_filter(), true, &[], &[], "testuser")
            .await
            .unwrap();

        assert_eq!(dirty.len(), 1, "N events collapse to one dirty-repo entry");
        assert!(dirty.contains(&("o".into(), "n".into())));

        let mut conn = pool.acquire().await.unwrap();
        let tasks = build_checkout_tasks(&mut *conn, &cfg, &dirty).await;

        let matching: Vec<_> = tasks.iter()
            .filter(|t| t.owner == "o" && t.name == "n" && t.is_dirty)
            .collect();
        assert_eq!(matching.len(), 1,
            "5 events must collapse to exactly one dirty CheckoutTask, got {}: {:?}",
            matching.len(),
            tasks.iter().map(|t| (&t.owner, &t.name, t.is_dirty)).collect::<Vec<_>>());
    }

    #[tokio::test]
    async fn zero_events_produce_no_dirty_checkout_task() {
        // Flip side of the cardinality invariant: silent repo => task present
        // (so first-run clone still works) but is_dirty=false => no fetch.
        let pool = db::open_in_memory().await.unwrap();
        let mock = MockGhClient::new();
        mock.push_rest_304();
        mock.push_graphql(serde_json::json!({"repo_0": {"pullRequests": {"nodes": []}}}));

        let mut cfg = cfg_with_repo("o", "n");
        cfg.repos[0].checkout_pr_branches = Some(true);

        let dirty = run(&pool, &mock, &cfg, all_filter(), true, &[], &[], "testuser")
            .await
            .unwrap();
        assert!(dirty.is_empty(), "304 events => no dirty repos");

        let mut conn = pool.acquire().await.unwrap();
        let tasks = build_checkout_tasks(&mut *conn, &cfg, &dirty).await;
        assert_eq!(tasks.len(), 1);
        assert!(!tasks[0].is_dirty, "no activity => is_dirty must be false");
    }

    #[tokio::test]
    async fn repo_without_checkout_flags_is_not_tasked() {
        // Second cardinality invariant: repos without any checkout_* flag never
        // generate a task even if dirty.
        let pool = db::open_in_memory().await.unwrap();
        let mut c = pool.acquire().await.unwrap();
        let _ = db::upsert_repo(&mut *c, "o", "n", "main").await.unwrap();
        drop(c);

        let cfg = cfg_with_repo("o", "n"); // no checkout flags set
        let mut dirty = HashSet::new();
        dirty.insert(("o".to_string(), "n".to_string()));

        let mut conn = pool.acquire().await.unwrap();
        let tasks = build_checkout_tasks(&mut *conn, &cfg, &dirty).await;
        assert_eq!(tasks.len(), 0, "repos without checkout_* flags must not be tasked");
    }

    #[tokio::test]
    async fn push_events_do_not_trigger_pr_sync() {
        let pool = db::open_in_memory().await.unwrap();
        let mock = MockGhClient::new();
        mock.push_rest(serde_json::json!([
            {"id": "9", "type": "PushEvent", "actor": {"login": "alice"},
             "payload": {"ref": "refs/heads/main"}, "created_at": "2026-01-01T00:00:00Z"},
        ]));

        run(&pool, &mock, &cfg_with_repo("o", "n"), all_filter(), false, &[], &[], "testuser").await.unwrap();

        assert_eq!(mock.graphql_call_count(), 0);
    }
}

#[cfg(test)]
mod integration {
    use super::*;
    use crate::config::{RepoConfig, ResolvedConfig};
    use crate::db;
    use crate::gh::GhClient;

    fn live_cfg(tmp: &std::path::Path) -> ResolvedConfig {
        ResolvedConfig {
            db_path:               tmp.join("test.db"),
            staging_folder:        tmp.to_path_buf(),
            poll_interval_seconds: 60,
            log_level:             "warn".into(),
            gh_binary:             "gh".into(),
            rate_warn_threshold:   500,
            rate_stop_threshold:   50,
            cmd_port:              7748,
            heartbeat_ttl_seconds: 30,
            sync_notifications:    false,
            repos: vec![RepoConfig {
                owner:                "hafley66".into(),
                name:                 "cc-hud".into(),
                default_branch:       Some("main".into()),
                sync_prs:             Some(true),
                sync_events:          Some(true),
                sync_notifications:   Some(false),
                sync_branches:        Some(vec!["main".into()]),
                checkout_on_sync:     None,
                checkout_pr_branches: None,
                fs_alias:             None,
            }],
            orgs: vec![],
            owner_fs_aliases: std::collections::HashMap::new(),
        }
    }

    fn all_filter() -> SyncFilter {
        SyncFilter { repo: None, prs_only: false, notifs_only: false, events_only: false }
    }

    #[tokio::test]
    #[ignore = "requires gh CLI authenticated"]
    async fn live_sync_and_cache_hit() {
        let tmp  = tempfile::tempdir().unwrap();
        let cfg  = live_cfg(tmp.path());
        let pool = db::open(&cfg.db_path).await.unwrap();
        let gh   = GhClient::new("gh", 500, 50, true, false);

        run(&pool, &gh, &cfg, all_filter(), true, &[], &[], "testuser").await.unwrap();

        let repo_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM repo WHERE owner='hafley66' AND name='cc-hud'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(repo_count, 1);

        let branch_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM branch b
             JOIN repo r ON r.id = b.repo_id
             WHERE r.owner='hafley66' AND r.name='cc-hud'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(branch_count >= 1, "no branches synced; got {branch_count}");

        let main_sha: String = sqlx::query_scalar(
            "SELECT b.sha FROM branch b
             JOIN repo r ON r.id = b.repo_id
             WHERE r.owner='hafley66' AND r.name='cc-hud' AND b.name='main'",
        )
        .fetch_one(&pool)
        .await
        .expect("main branch not in DB");
        assert!(!main_sha.is_empty());

        let rest_calls: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM call_log WHERE api_type='rest'")
            .fetch_one(&pool).await.unwrap();
        assert!(rest_calls > 0);

        let change_rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM change_log")
            .fetch_one(&pool).await.unwrap();
        assert!(change_rows > 0);

        run(&pool, &gh, &cfg, all_filter(), false, &[], &[], "testuser").await.unwrap();

        let cache_hits: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM call_log WHERE cache_hit=1")
            .fetch_one(&pool).await.unwrap();
        assert!(cache_hits > 0);

        let main_sha2: String = sqlx::query_scalar(
            "SELECT b.sha FROM branch b
             JOIN repo r ON r.id = b.repo_id
             WHERE r.owner='hafley66' AND r.name='cc-hud' AND b.name='main'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(main_sha, main_sha2);
    }
}
