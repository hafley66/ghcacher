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
use crate::gh::{GhRequest, GitHubClient};

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
            poll_interval_seconds: org.poll_interval_seconds,
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
) -> Result<()> {
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
                match events::sync_org(&mut *conn, gh, &org.owner).await {
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

    for repo in all_repos {
        if let Some(ref slug) = filter.repo {
            if &repo.slug() != slug { continue; }
        }

        tracing::info!(repo = %repo.slug(), full_sweep, "syncing");

        // Throttle outside the per-repo transaction.
        {
            let mut c = pool.acquire().await?;
            gh.throttle_if_needed(&mut *c, "rest").await?;
        }

        let mut tx = pool.begin().await?;
        let mut dirty_prs: Vec<i64> = vec![];

        let result: Result<()> = async {
            let default_branch = repo.default_branch.as_deref().unwrap_or("main");
            let repo_id = db::upsert_repo(&mut *tx, &repo.owner, &repo.name, default_branch).await?;

            if filter.do_events() && repo.sync_events.unwrap_or(false) {
                if org_owners.contains(&repo.owner) {
                    dirty_prs = org_dirty
                        .get(&(repo.owner.clone(), repo.name.clone()))
                        .cloned()
                        .unwrap_or_default();
                } else {
                    dirty_prs = events::sync(&mut *tx, gh, repo_id, &repo.owner, &repo.name).await?;
                }
            }

            if filter.do_prs() && repo.sync_prs.unwrap_or(false) {
                if full_sweep || filter.prs_only {
                    prs::sync(&mut *tx, gh, repo_id, &repo.owner, &repo.name).await?;
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
        }
    }

    let notifs_needed = filter.do_notifs()
        && (cfg.sync_notifications
            || cfg.repos.iter().any(|r| r.sync_notifications.unwrap_or(false))
            || !extra_notif_slugs.is_empty());
    if notifs_needed {
        let mut conn = pool.acquire().await?;
        notifications::sync(&mut *conn, gh, cfg, extra_notif_slugs).await?;
    }

    Ok(())
}

pub async fn watch(
    pool: &SqlitePool,
    gh: &dyn GitHubClient,
    cfg: &ResolvedConfig,
    subs: Option<std::sync::Arc<crate::cmd::Subscriptions>>,
    paused: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> Result<()> {
    tracing::info!("starting watch loop");
    let mut first_run = true;
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
                        poll_interval_seconds: None,
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

        if let Err(e) = run(pool, gh, cfg, filter, first_run, &extra_repos, &extra_notif_slugs).await {
            tracing::error!(error = %e, "watch sync error");
        }
        first_run = false;

        // Checkout tasks.
        {
            let mut conn = pool.acquire().await?;
            let mut tasks: Vec<checkout::CheckoutTask> = vec![];
            for r in cfg.repos.iter().filter(|r| r.checkout_on_sync.unwrap_or(false)) {
                let repo_id = match db::get_repo_id(&mut *conn, &r.owner, &r.name).await {
                    Ok(Some(id)) => id,
                    _ => continue,
                };
                for b in r.sync_branches.as_deref().unwrap_or(&[]) {
                    tasks.push(checkout::CheckoutTask {
                        repo_id,
                        owner:    r.owner.clone(),
                        name:     r.name.clone(),
                        branch:   b.clone(),
                        fs_owner: r.fs_owner().to_owned(),
                    });
                }
            }

            for org in cfg.orgs.iter().filter(|o| o.checkout_on_sync.unwrap_or(false)) {
                let rows: Vec<(i64, String)> = sqlx::query_as(
                    "SELECT id, name FROM repo WHERE owner = ?",
                )
                .bind(&org.owner)
                .fetch_all(&mut *conn)
                .await
                .unwrap_or_default();
                for (repo_id, name) in rows {
                    for branch in org.sync_branches.as_deref().unwrap_or(&[]) {
                        tasks.push(checkout::CheckoutTask {
                            repo_id,
                            owner:    org.owner.clone(),
                            name:     name.clone(),
                            branch:   branch.clone(),
                            fs_owner: org.fs_owner().to_owned(),
                        });
                    }
                }
            }

            if !tasks.is_empty() {
                if let Err(e) = checkout::checkout_all(pool, &cfg.staging_folder, &tasks).await {
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
                poll_interval_seconds: None,
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
        mock.push_rest(serde_json::json!([]));
        mock.push_graphql(serde_json::json!({"repository": {"pullRequests": {"nodes": []}}}));

        run(&pool, &mock, &cfg_with_repo("o", "n"), all_filter(), true, &[], &[]).await.unwrap();

        assert_eq!(mock.graphql_call_count(), 1);
        let (endpoint, _) = &mock.graphql_calls.lock().unwrap()[0];
        assert!(endpoint.contains("full"), "expected full endpoint, got {endpoint}");
    }

    #[tokio::test]
    async fn targeted_no_events_skips_graphql() {
        let pool = db::open_in_memory().await.unwrap();
        let mock = MockGhClient::new();
        mock.push_rest_304();

        run(&pool, &mock, &cfg_with_repo("o", "n"), all_filter(), false, &[], &[]).await.unwrap();

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

        run(&pool, &mock, &cfg_with_repo("o", "n"), all_filter(), false, &[], &[]).await.unwrap();

        assert_eq!(mock.graphql_call_count(), 1, "expected exactly 1 batched graphql call");
        let (endpoint, query) = &mock.graphql_calls.lock().unwrap()[0];
        assert!(endpoint.contains("targeted"), "expected targeted endpoint, got {endpoint}");
        assert!(query.contains("pr_42"), "expected alias pr_42 in query");
        assert!(query.contains("pr_17"), "expected alias pr_17 in query");
    }

    #[tokio::test]
    async fn push_events_do_not_trigger_pr_sync() {
        let pool = db::open_in_memory().await.unwrap();
        let mock = MockGhClient::new();
        mock.push_rest(serde_json::json!([
            {"id": "9", "type": "PushEvent", "actor": {"login": "alice"},
             "payload": {"ref": "refs/heads/main"}, "created_at": "2026-01-01T00:00:00Z"},
        ]));

        run(&pool, &mock, &cfg_with_repo("o", "n"), all_filter(), false, &[], &[]).await.unwrap();

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
                poll_interval_seconds: None,
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

        run(&pool, &gh, &cfg, all_filter(), true, &[], &[]).await.unwrap();

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

        run(&pool, &gh, &cfg, all_filter(), false, &[], &[]).await.unwrap();

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
