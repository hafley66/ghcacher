pub mod branches;
pub mod events;
pub mod notifications;
pub mod prs;

use anyhow::Result;
use rusqlite::Connection;
use std::time::Duration;

use crate::checkout;
use crate::config::{OrgConfig, RepoConfig, ResolvedConfig};
use crate::db;
use crate::gh::{GhRequest, GitHubClient};

pub struct SyncFilter {
    pub repo: Option<String>,
    pub prs_only: bool,
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

/// Sentinel stored in poll_state.etag when we've confirmed owner is a user, not an org.
const NOT_ORG: &str = "__user__";

/// Discover all repo names under a GitHub org or user account.
/// Tries /orgs/{owner}/repos first; falls back to /users/{owner}/repos on 404.
/// The 404 result is memoized in poll_state so the org probe only fires once.
/// ETag-gated and paginated -- typically a free 304 after the first call.
fn discover_org_repos(conn: &Connection, gh: &dyn GitHubClient, owner: &str) -> Result<Vec<String>> {
    let org_endpoint = format!("/orgs/{owner}/repos");
    let user_endpoint = format!("/users/{owner}/repos");

    let poll = db::get_poll_state(conn, &org_endpoint)?;

    let body = if poll.etag.as_deref() == Some(NOT_ORG) {
        // Already confirmed this owner is a user account; skip the org probe.
        let poll2 = db::get_poll_state(conn, &user_endpoint)?;
        let mut req2 = GhRequest::get(&user_endpoint).paginated();
        if let Some(ref etag) = poll2.etag {
            req2 = req2.with_etag(etag);
        }
        let resp2 = gh.call(conn, &req2)?;
        if resp2.is_not_modified() {
            return Ok(vec![]);
        }
        resp2.body
    } else {
        let mut req = GhRequest::get(&org_endpoint).paginated();
        if let Some(ref etag) = poll.etag {
            req = req.with_etag(etag);
        }
        let resp = gh.call(conn, &req)?;

        if resp.status == 404 {
            // Not an org account; memoize so we skip this probe on future polls.
            db::set_poll_state(conn, &org_endpoint, Some(NOT_ORG), None, None, false)?;
            let poll2 = db::get_poll_state(conn, &user_endpoint)?;
            let mut req2 = GhRequest::get(&user_endpoint).paginated();
            if let Some(ref etag) = poll2.etag {
                req2 = req2.with_etag(etag);
            }
            let resp2 = gh.call(conn, &req2)?;
            if resp2.is_not_modified() {
                return Ok(vec![]);
            }
            resp2.body
        } else if resp.is_not_modified() {
            return Ok(vec![]);
        } else {
            resp.body
        }
    };

    let names: Vec<String> = body
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|r| r["name"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();

    tracing::info!(owner, count = names.len() as u64, "discovered org repos");
    Ok(names)
}

fn org_to_repos(org: &OrgConfig, names: Vec<String>) -> Vec<RepoConfig> {
    names
        .into_iter()
        .filter(|n| !org.exclude.contains(n))
        .map(|name| RepoConfig {
            owner: org.owner.clone(),
            name,
            default_branch: None,
            sync_prs: org.sync_prs,
            sync_notifications: org.sync_notifications,
            sync_events: org.sync_events,
            sync_branches: org.sync_branches.clone(),
            checkout_on_sync: org.checkout_on_sync,
            poll_interval_seconds: org.poll_interval_seconds,
        })
        .collect()
}

/// `full_sweep` forces a full GraphQL fetch of all open PRs instead of
/// using event hints to target only changed PRs. Always true for one-shot
/// `ghcache sync`; true only on the first iteration of `ghcache watch`.
pub fn run(
    conn: &Connection,
    gh: &dyn GitHubClient,
    cfg: &ResolvedConfig,
    filter: SyncFilter,
    full_sweep: bool,
    extra_repos: &[RepoConfig],
    extra_notif_slugs: &[(String, String)],
) -> Result<()> {
    // Expand [[org]] entries into synthetic RepoConfigs.
    let mut org_repos: Vec<RepoConfig> = vec![];
    for org in &cfg.orgs {
        match discover_org_repos(conn, gh, &org.owner) {
            Ok(names) => org_repos.extend(org_to_repos(org, names)),
            Err(e) => tracing::error!(owner = %org.owner, error = %e, "org repo discovery failed"),
        }
    }

    let all_repos: Vec<&RepoConfig> = cfg.repos.iter().chain(org_repos.iter()).chain(extra_repos.iter()).collect();

    for repo in all_repos {
        if let Some(ref slug) = filter.repo {
            if &repo.slug() != slug {
                continue;
            }
        }

        tracing::info!(repo = %repo.slug(), full_sweep, "syncing");

        gh.throttle_if_needed(conn, "rest")?;

        let tx = conn.unchecked_transaction()?;

        let mut dirty_prs: Vec<i64> = vec![];

        let result = (|| -> Result<()> {
            let default_branch = repo.default_branch.as_deref().unwrap_or("main");
            let repo_id = db::upsert_repo(conn, &repo.owner, &repo.name, default_branch)?;

            // Events first so their PR-number hints are available before PR sync.
            if filter.do_events() && repo.sync_events.unwrap_or(false) {
                dirty_prs = events::sync(conn, gh, repo_id, &repo.owner, &repo.name)?;
            }

            if filter.do_prs() && repo.sync_prs.unwrap_or(false) {
                if full_sweep || filter.prs_only {
                    prs::sync(conn, gh, repo_id, &repo.owner, &repo.name)?;
                } else {
                    prs::sync_targeted(conn, gh, repo_id, &repo.owner, &repo.name, &dirty_prs)?;
                }
            }

            if repo.sync_branches.as_ref().map(|b| !b.is_empty()).unwrap_or(false) {
                if filter.all() {
                    branches::sync(conn, gh, repo_id, &repo.owner, &repo.name, repo.sync_branches.as_deref().unwrap_or(&[]))?;
                }
            }

            Ok(())
        })();

        if let Err(e) = result {
            tracing::error!(repo = %repo.slug(), error = %e, "sync failed, rolling back");
            tx.rollback()?;
        } else {
            tx.commit()?;
        }
    }

    // Notifications are cross-repo, sync once.
    let notifs_needed = filter.do_notifs()
        && (cfg.repos.iter().any(|r| r.sync_notifications.unwrap_or(false))
            || !extra_notif_slugs.is_empty());
    if notifs_needed {
        notifications::sync(conn, gh, cfg, extra_notif_slugs)?;
    }

    Ok(())
}

pub fn watch(
    conn: &Connection,
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
            std::thread::sleep(Duration::from_millis(500));
            continue;
        }

        let filter = SyncFilter {
            repo: None,
            prs_only: false,
            notifs_only: false,
            events_only: false,
        };

        // Repos subscribed via the cmd server that aren't already in config.
        let (extra_repos, extra_notif_slugs): (Vec<RepoConfig>, Vec<(String, String)>) = subs
            .as_ref()
            .map(|s| {
                let pr_repos = s.active_pr_sync_repos();
                let notif_repos = s.active_notifications_repos();

                let extra_repos = pr_repos
                    .into_iter()
                    .filter(|(o, n)| !cfg.repos.iter().any(|r| &r.owner == o && &r.name == n))
                    .map(|(owner, name)| RepoConfig {
                        owner,
                        name,
                        default_branch: None,
                        sync_prs: Some(true),
                        sync_notifications: None,
                        sync_events: None,
                        sync_branches: None,
                        checkout_on_sync: None,
                        poll_interval_seconds: None,
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

        if let Err(e) = run(conn, gh, cfg, filter, first_run, &extra_repos, &extra_notif_slugs) {
            tracing::error!(error = %e, "watch sync error");
        }
        first_run = false;

        {
            let staging = &cfg.staging_folder;
            let mut tasks: Vec<checkout::CheckoutTask> = cfg.repos.iter()
                .filter(|r| r.checkout_on_sync.unwrap_or(false))
                .flat_map(|r| {
                    let repo_id = match db::get_repo_id(conn, &r.owner, &r.name) {
                        Ok(Some(id)) => id,
                        _ => return vec![],
                    };
                    r.sync_branches.as_deref().unwrap_or(&[]).iter().map(|b| checkout::CheckoutTask {
                        repo_id,
                        owner: r.owner.clone(),
                        name: r.name.clone(),
                        branch: b.clone(),
                    }).collect()
                })
                .collect();

            // Org repos were upserted into the DB during sync; query by owner.
            for org in cfg.orgs.iter().filter(|o| o.checkout_on_sync.unwrap_or(false)) {
                let mut stmt = conn.prepare_cached(
                    "SELECT id, name FROM repo WHERE owner = ?1"
                ).unwrap();
                let rows: Vec<(i64, String)> = stmt
                    .query_map(rusqlite::params![org.owner], |r| Ok((r.get(0)?, r.get(1)?)))
                    .unwrap()
                    .filter_map(|r| r.ok())
                    .collect();
                for (repo_id, name) in rows {
                    for branch in org.sync_branches.as_deref().unwrap_or(&[]) {
                        tasks.push(checkout::CheckoutTask {
                            repo_id,
                            owner: org.owner.clone(),
                            name: name.clone(),
                            branch: branch.clone(),
                        });
                    }
                }
            }

            if !tasks.is_empty() {
                if let Err(e) = checkout::checkout_all(conn, staging, &tasks) {
                    tracing::error!(error = %e, "checkout_all failed");
                }
            }
        }

        let interval = cfg.poll_interval_seconds;
        tracing::debug!(sleep_seconds = interval, "watch sleeping");
        std::thread::sleep(Duration::from_secs(interval));
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
            db_path: std::path::PathBuf::from(":memory:"),
            staging_folder: std::path::PathBuf::from("/tmp"),
            poll_interval_seconds: 60,
            log_level: "info".into(),
            gh_binary: "gh".into(),
            rate_warn_threshold: 500,
            rate_stop_threshold: 50,
            cmd_port: 7748,
            heartbeat_ttl_seconds: 30,
            repos: vec![RepoConfig {
                owner: owner.into(),
                name: name.into(),
                default_branch: Some("main".into()),
                sync_prs: Some(true),
                sync_events: Some(true),
                sync_notifications: Some(false),
                sync_branches: None,
                checkout_on_sync: None,
                poll_interval_seconds: None,
            }],
            orgs: vec![],
        }
    }

    fn all_filter() -> SyncFilter {
        SyncFilter { repo: None, prs_only: false, notifs_only: false, events_only: false }
    }

    /// Full sweep: graphql called once with the full PR list endpoint.
    #[test]
    fn full_sweep_calls_graphql_once() {
        let conn = db::open_in_memory().unwrap();
        let mock = MockGhClient::new();
        // events returns 200 empty array; graphql returns empty PR nodes
        mock.push_rest(serde_json::json!([]));
        mock.push_graphql(serde_json::json!({"repository": {"pullRequests": {"nodes": []}}}));

        run(&conn, &mock, &cfg_with_repo("o", "n"), all_filter(), true, &[], &[]).unwrap();

        assert_eq!(mock.graphql_call_count(), 1);
        let (endpoint, _) = &mock.graphql_calls.borrow()[0];
        assert!(endpoint.contains("full"), "expected full endpoint, got {endpoint}");
    }

    /// No new events (304) → zero graphql calls on targeted path.
    #[test]
    fn targeted_no_events_skips_graphql() {
        let conn = db::open_in_memory().unwrap();
        let mock = MockGhClient::new();
        mock.push_rest_304(); // events 304

        run(&conn, &mock, &cfg_with_repo("o", "n"), all_filter(), false, &[], &[]).unwrap();

        assert_eq!(mock.graphql_call_count(), 0);
    }

    /// Two PR events → exactly one batched graphql call labeled "targeted".
    #[test]
    fn targeted_batches_multiple_prs_in_one_call() {
        let conn = db::open_in_memory().unwrap();
        let mock = MockGhClient::new();
        mock.push_rest(serde_json::json!([
            {"id": "1", "type": "PullRequestEvent", "actor": {"login": "alice"},
             "payload": {"number": 42, "action": "opened"}, "created_at": "2026-01-01T00:00:00Z"},
            {"id": "2", "type": "PullRequestReviewEvent", "actor": {"login": "bob"},
             "payload": {"action": "submitted", "pull_request": {"number": 17}},
             "created_at": "2026-01-01T00:00:01Z"},
        ]));
        // targeted graphql call returns empty repo (no PRs to upsert)
        mock.push_graphql(serde_json::json!({"repository": {}}));

        run(&conn, &mock, &cfg_with_repo("o", "n"), all_filter(), false, &[], &[]).unwrap();

        assert_eq!(mock.graphql_call_count(), 1, "expected exactly 1 batched graphql call");
        let (endpoint, query) = &mock.graphql_calls.borrow()[0];
        assert!(endpoint.contains("targeted"), "expected targeted endpoint, got {endpoint}");
        assert!(query.contains("pr_42"), "expected alias pr_42 in query");
        assert!(query.contains("pr_17"), "expected alias pr_17 in query");
    }

    /// Non-PR events don't trigger any graphql call.
    #[test]
    fn push_events_do_not_trigger_pr_sync() {
        let conn = db::open_in_memory().unwrap();
        let mock = MockGhClient::new();
        mock.push_rest(serde_json::json!([
            {"id": "9", "type": "PushEvent", "actor": {"login": "alice"},
             "payload": {"ref": "refs/heads/main"}, "created_at": "2026-01-01T00:00:00Z"},
        ]));

        run(&conn, &mock, &cfg_with_repo("o", "n"), all_filter(), false, &[], &[]).unwrap();

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
            db_path: tmp.join("test.db"),
            staging_folder: tmp.to_path_buf(),
            poll_interval_seconds: 60,
            log_level: "warn".into(),
            gh_binary: "gh".into(),
            rate_warn_threshold: 500,
            rate_stop_threshold: 50,
            cmd_port: 7748,
            heartbeat_ttl_seconds: 30,
            repos: vec![RepoConfig {
                owner: "hafley66".into(),
                name: "cc-hud".into(),
                default_branch: Some("main".into()),
                sync_prs: Some(true),
                sync_events: Some(true),
                sync_notifications: Some(false),
                sync_branches: Some(vec!["main".into()]),
                checkout_on_sync: None,
                poll_interval_seconds: None,
            }],
            orgs: vec![],
        }
    }

    fn all_filter() -> SyncFilter {
        SyncFilter { repo: None, prs_only: false, notifs_only: false, events_only: false }
    }

    /// Full sync against real hafley66/cc-hud, then a second pass to confirm 304 cache hits.
    /// Run with: cargo test integration -- --include-ignored
    #[test]
    #[ignore = "requires gh CLI authenticated"]
    fn live_sync_and_cache_hit() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = live_cfg(tmp.path());
        let conn = db::open(&cfg.db_path).unwrap();
        let gh = GhClient::new("gh", 500, 50, true, false); // silent=true: no stdout noise in test

        // --- first pass: full sweep ---
        run(&conn, &gh, &cfg, all_filter(), true, &[], &[]).unwrap();

        let repo_count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM repo WHERE owner='hafley66' AND name='cc-hud'",
            [], |r| r.get(0),
        ).unwrap();
        assert_eq!(repo_count, 1, "repo not inserted");

        let branch_count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM branch b
             JOIN repo r ON r.id = b.repo_id
             WHERE r.owner='hafley66' AND r.name='cc-hud'",
            [], |r| r.get(0),
        ).unwrap();
        assert!(branch_count >= 1, "no branches synced; got {branch_count}");

        let main_sha: String = conn.query_row(
            "SELECT b.sha FROM branch b
             JOIN repo r ON r.id = b.repo_id
             WHERE r.owner='hafley66' AND r.name='cc-hud' AND b.name='main'",
            [], |r| r.get(0),
        ).expect("main branch not in DB");
        assert!(!main_sha.is_empty(), "main branch sha is empty");

        let rest_calls: i64 = conn.query_row(
            "SELECT COUNT(*) FROM call_log WHERE api_type='rest'",
            [], |r| r.get(0),
        ).unwrap();
        assert!(rest_calls > 0, "no REST calls logged");

        let change_rows: i64 = conn.query_row(
            "SELECT COUNT(*) FROM change_log",
            [], |r| r.get(0),
        ).unwrap();
        assert!(change_rows > 0, "change_log empty after first sync");

        // --- second pass: should be all 304s ---
        run(&conn, &gh, &cfg, all_filter(), false, &[], &[]).unwrap();

        let cache_hits: i64 = conn.query_row(
            "SELECT COUNT(*) FROM call_log WHERE cache_hit=1",
            [], |r| r.get(0),
        ).unwrap();
        assert!(cache_hits > 0, "second sync had no cache hits (expected 304s)");

        // sha is stable across passes
        let main_sha2: String = conn.query_row(
            "SELECT b.sha FROM branch b
             JOIN repo r ON r.id = b.repo_id
             WHERE r.owner='hafley66' AND r.name='cc-hud' AND b.name='main'",
            [], |r| r.get(0),
        ).unwrap();
        assert_eq!(main_sha, main_sha2, "main branch sha changed between syncs unexpectedly");
    }
}
