pub mod branches;
pub mod events;
pub mod notifications;
pub mod prs;

use anyhow::Result;
use rusqlite::Connection;
use std::time::Duration;

use crate::config::ResolvedConfig;
use crate::db;
use crate::gh::GhClient;

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

pub fn run(conn: &Connection, gh: &GhClient, cfg: &ResolvedConfig, filter: SyncFilter) -> Result<()> {
    for repo in &cfg.repos {
        if let Some(ref slug) = filter.repo {
            if &repo.slug() != slug {
                continue;
            }
        }

        tracing::info!(repo = %repo.slug(), "syncing");

        // Back off if rate limit is low before starting this repo's calls
        gh.throttle_if_needed(conn, "rest")?;

        let tx = conn.unchecked_transaction()?;

        let result = (|| -> Result<()> {
            let default_branch = repo.default_branch.as_deref().unwrap_or("main");
            let repo_id = db::upsert_repo(conn, &repo.owner, &repo.name, default_branch)?;

            if filter.do_prs() && repo.sync_prs.unwrap_or(false) {
                prs::sync(conn, gh, repo_id, &repo.owner, &repo.name)?;
            }

            if filter.do_events() && repo.sync_events.unwrap_or(false) {
                events::sync(conn, gh, repo_id, &repo.owner, &repo.name)?;
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

pub fn watch(conn: &Connection, gh: &GhClient, cfg: &ResolvedConfig) -> Result<()> {
    tracing::info!("starting watch loop");
    loop {
        let filter = SyncFilter {
            repo: None,
            prs_only: false,
            notifs_only: false,
            events_only: false,
        };
        if let Err(e) = run(conn, gh, cfg, filter) {
            tracing::error!(error = %e, "watch sync error");
        }

        let interval = cfg.poll_interval_seconds;
        tracing::debug!(sleep_seconds = interval, "watch sleeping");
        std::thread::sleep(Duration::from_secs(interval));
    }
}
