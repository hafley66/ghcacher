pub mod branches;
pub mod events;
pub mod notifications;
pub mod prs;

use anyhow::Result;
use rusqlite::Connection;
use std::time::Duration;

use crate::config::ResolvedConfig;
use crate::db;
use crate::gh::GitHubClient;

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

/// `full_sweep` forces a full GraphQL fetch of all open PRs instead of
/// using event hints to target only changed PRs. Always true for one-shot
/// `ghcache sync`; true only on the first iteration of `ghcache watch`.
pub fn run(conn: &Connection, gh: &dyn GitHubClient, cfg: &ResolvedConfig, filter: SyncFilter, full_sweep: bool) -> Result<()> {
    for repo in &cfg.repos {
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

    // Notifications are cross-repo, sync once
    if filter.do_notifs() && cfg.repos.iter().any(|r| r.sync_notifications.unwrap_or(false)) {
        notifications::sync(conn, gh, cfg)?;
    }

    Ok(())
}

pub fn watch(conn: &Connection, gh: &dyn GitHubClient, cfg: &ResolvedConfig) -> Result<()> {
    tracing::info!("starting watch loop");
    let mut first_run = true;
    loop {
        let filter = SyncFilter {
            repo: None,
            prs_only: false,
            notifs_only: false,
            events_only: false,
        };
        if let Err(e) = run(conn, gh, cfg, filter, first_run) {
            tracing::error!(error = %e, "watch sync error");
        }
        first_run = false;

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
            staging_folder: None,
            poll_interval_seconds: 60,
            log_level: "info".into(),
            gh_binary: "gh".into(),
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

        run(&conn, &mock, &cfg_with_repo("o", "n"), all_filter(), true).unwrap();

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

        run(&conn, &mock, &cfg_with_repo("o", "n"), all_filter(), false).unwrap();

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

        run(&conn, &mock, &cfg_with_repo("o", "n"), all_filter(), false).unwrap();

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

        run(&conn, &mock, &cfg_with_repo("o", "n"), all_filter(), false).unwrap();

        assert_eq!(mock.graphql_call_count(), 0);
    }
}
