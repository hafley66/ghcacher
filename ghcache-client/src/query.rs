use anyhow::{Context, Result};
use rusqlite::{Connection, params};
use std::path::Path;

use crate::types::*;

pub struct Client {
    conn: Connection,
}

impl Client {
    /// Open the ghcache database read-only.
    pub fn open(db_path: &Path) -> Result<Self> {
        let conn = Connection::open_with_flags(
            db_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .with_context(|| format!("opening ghcache db at {}", db_path.display()))?;
        conn.execute_batch("PRAGMA foreign_keys = ON; PRAGMA journal_mode = WAL;")?;
        Ok(Client { conn })
    }

    /// Open from a read-write connection (for testing against in-memory DBs).
    pub fn from_conn(conn: Connection) -> Self {
        Client { conn }
    }

    // ---- Pull Requests --------------------------------------------------

    pub fn prs(&self, filter: &PrFilter) -> Result<Vec<PullRequest>> {
        let mut conditions = vec![];

        let state = filter.state.unwrap_or("open");
        conditions.push(format!("pr.state = '{}'", esc(state)));

        if let Some(repo) = filter.repo {
            let (owner, name) = split_slug(repo)?;
            conditions.push(format!("r.owner = '{}' AND r.name = '{}'", esc(owner), esc(name)));
        }

        if let Some(author) = filter.author {
            conditions.push(format!("pr.author = '{}'", esc(author)));
        }

        if filter.needs_review {
            conditions.push(
                "(SELECT COUNT(*) FROM pr_review rv WHERE rv.pr_id = pr.id AND rv.state = 'APPROVED') = 0"
                    .into(),
            );
        }

        let where_clause = if conditions.is_empty() {
            "1=1".into()
        } else {
            conditions.join(" AND ")
        };

        let sql = format!(
            "SELECT pr.id, r.owner || '/' || r.name, pr.number, pr.title, pr.author,
                    pr.head_ref, pr.base_ref, pr.state, pr.draft, pr.mergeable,
                    pr.additions, pr.deletions, pr.changed_files,
                    pr.created_at, pr.updated_at, pr.merged_at, pr.body
             FROM pull_request pr
             JOIN repo r ON r.id = pr.repo_id
             WHERE {where_clause}
             ORDER BY pr.updated_at DESC"
        );

        let mut stmt = self.conn.prepare(&sql)?;
        let rows: Vec<PullRequest> = stmt.query_map([], |r| {
            Ok(PullRequest {
                id: r.get(0)?,
                repo_slug: r.get(1)?,
                number: r.get(2)?,
                title: r.get(3)?,
                author: r.get(4)?,
                head_ref: r.get(5)?,
                base_ref: r.get(6)?,
                state: r.get(7)?,
                draft: r.get::<_, i64>(8)? != 0,
                mergeable: r.get(9)?,
                additions: r.get(10)?,
                deletions: r.get(11)?,
                changed_files: r.get(12)?,
                created_at: r.get(13)?,
                updated_at: r.get(14)?,
                merged_at: r.get(15)?,
                body: r.get(16)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn pr(&self, repo: &str, number: i64) -> Result<Option<PullRequest>> {
        let (owner, name) = split_slug(repo)?;
        let mut stmt = self.conn.prepare_cached(
            "SELECT pr.id, r.owner || '/' || r.name, pr.number, pr.title, pr.author,
                    pr.head_ref, pr.base_ref, pr.state, pr.draft, pr.mergeable,
                    pr.additions, pr.deletions, pr.changed_files,
                    pr.created_at, pr.updated_at, pr.merged_at, pr.body
             FROM pull_request pr
             JOIN repo r ON r.id = pr.repo_id
             WHERE r.owner = ?1 AND r.name = ?2 AND pr.number = ?3",
        )?;
        let mut rows = stmt.query(params![owner, name, number])?;
        rows.next()?
            .map(|r| -> rusqlite::Result<PullRequest> { Ok(PullRequest {
                id: r.get(0)?,
                repo_slug: r.get(1)?,
                number: r.get(2)?,
                title: r.get(3)?,
                author: r.get(4)?,
                head_ref: r.get(5)?,
                base_ref: r.get(6)?,
                state: r.get(7)?,
                draft: r.get::<_, i64>(8)? != 0,
                mergeable: r.get(9)?,
                additions: r.get(10)?,
                deletions: r.get(11)?,
                changed_files: r.get(12)?,
                created_at: r.get(13)?,
                updated_at: r.get(14)?,
                merged_at: r.get(15)?,
                body: r.get(16)?,
            }) })
            .transpose()
            .map_err(Into::into)
    }

    pub fn reviews(&self, pr_id: i64) -> Result<Vec<Review>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT id, pr_id, author, state, body, submitted_at
             FROM pr_review WHERE pr_id = ?1 ORDER BY submitted_at",
        )?;
        let rows: Vec<Review> = stmt.query_map(params![pr_id], |r| {
            Ok(Review {
                id: r.get(0)?,
                pr_id: r.get(1)?,
                author: r.get(2)?,
                state: r.get(3)?,
                body: r.get(4)?,
                submitted_at: r.get(5)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn comments(&self, pr_id: i64) -> Result<Vec<Comment>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT id, pr_id, author, body, path, line, created_at
             FROM pr_comment WHERE pr_id = ?1 ORDER BY created_at",
        )?;
        let rows: Vec<Comment> = stmt.query_map(params![pr_id], |r| {
            Ok(Comment {
                id: r.get(0)?,
                pr_id: r.get(1)?,
                author: r.get(2)?,
                body: r.get(3)?,
                path: r.get(4)?,
                line: r.get(5)?,
                created_at: r.get(6)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    // ---- Notifications --------------------------------------------------

    pub fn unread_notifications(&self) -> Result<Vec<Notification>> {
        self.notifications(true)
    }

    pub fn notifications(&self, unread_only: bool) -> Result<Vec<Notification>> {
        let filter = if unread_only { "WHERE n.unread = 1" } else { "" };
        let sql = format!(
            "SELECT n.id, n.gh_id, r.owner || '/' || r.name, n.subject_type,
                    n.subject_title, n.subject_url, n.reason, n.unread, n.updated_at
             FROM notification n
             LEFT JOIN repo r ON r.id = n.repo_id
             {filter}
             ORDER BY n.updated_at DESC"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows: Vec<Notification> = stmt.query_map([], |r| {
            Ok(Notification {
                id: r.get(0)?,
                gh_id: r.get(1)?,
                repo_slug: r.get(2)?,
                subject_type: r.get(3)?,
                subject_title: r.get(4)?,
                subject_url: r.get(5)?,
                reason: r.get(6)?,
                unread: r.get::<_, i64>(7)? != 0,
                updated_at: r.get(8)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    // ---- Branches -------------------------------------------------------

    pub fn branches(&self, repo: Option<&str>) -> Result<Vec<Branch>> {
        let where_clause = match repo {
            Some(r) => {
                let (owner, name) = split_slug(r)?;
                format!("WHERE r.owner = '{}' AND r.name = '{}'", esc(owner), esc(name))
            }
            None => String::new(),
        };
        let sql = format!(
            "SELECT b.id, r.owner || '/' || r.name, b.name, b.sha,
                    b.behind_default, b.ahead_default, b.updated_at
             FROM branch b JOIN repo r ON r.id = b.repo_id
             {where_clause}
             ORDER BY r.owner, r.name, b.name"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows: Vec<Branch> = stmt.query_map([], |r| {
            Ok(Branch {
                id: r.get(0)?,
                repo_slug: r.get(1)?,
                name: r.get(2)?,
                sha: r.get(3)?,
                behind_default: r.get(4)?,
                ahead_default: r.get(5)?,
                updated_at: r.get(6)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    // ---- Events ---------------------------------------------------------

    pub fn events(&self, repo: Option<&str>, event_type: Option<&str>, limit: usize) -> Result<Vec<RepoEvent>> {
        let mut conditions = vec!["1=1".to_owned()];
        if let Some(r) = repo {
            let (owner, name) = split_slug(r)?;
            conditions.push(format!("r.owner = '{}' AND r.name = '{}'", esc(owner), esc(name)));
        }
        if let Some(t) = event_type {
            conditions.push(format!("e.type = '{}'", esc(t)));
        }
        let where_clause = conditions.join(" AND ");
        let sql = format!(
            "SELECT e.id, r.owner || '/' || r.name, e.gh_id, e.type, e.actor,
                    e.payload_json, e.created_at
             FROM repo_event e JOIN repo r ON r.id = e.repo_id
             WHERE {where_clause}
             ORDER BY e.created_at DESC LIMIT {}",
            limit
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows: Vec<RepoEvent> = stmt.query_map([], |r| {
            let payload_str: Option<String> = r.get(5)?;
            Ok(RepoEvent {
                id: r.get(0)?,
                repo_slug: r.get(1)?,
                gh_id: r.get(2)?,
                event_type: r.get(3)?,
                actor: r.get(4)?,
                payload: payload_str
                    .as_deref()
                    .and_then(|s| serde_json::from_str(s).ok())
                    .unwrap_or(serde_json::Value::Null),
                created_at: r.get(6)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    // ---- Rate limit -----------------------------------------------------

    pub fn rate_limit(&self) -> Result<Vec<RateLimit>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT api_type,
                    rate_remaining,
                    datetime(rate_reset, 'unixepoch') AS reset_at,
                    gql_cost
             FROM call_log
             WHERE id IN (
                 SELECT MAX(id) FROM call_log WHERE rate_remaining IS NOT NULL GROUP BY api_type
             )",
        )?;
        let rows: Vec<RateLimit> = stmt.query_map([], |r| {
            Ok(RateLimit {
                api_type: r.get(0)?,
                remaining: r.get(1)?,
                reset_at: r.get(2)?,
                gql_cost: r.get(3)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    // ---- Change log (for polling, prefer Subscriber for push) -----------

    pub fn changes_since(&self, last_id: i64) -> Result<Vec<crate::tail::ChangeEvent>> {
        crate::tail::poll(&self.conn, last_id)
    }

    pub fn latest_change_id(&self) -> Result<i64> {
        self.conn
            .query_row("SELECT COALESCE(MAX(id), 0) FROM change_log", [], |r| r.get(0))
            .map_err(Into::into)
    }
}

fn esc(s: &str) -> String {
    s.replace('\'', "''")
}

fn split_slug(slug: &str) -> Result<(&str, &str)> {
    let mut parts = slug.splitn(2, '/');
    let owner = parts.next().context("slug missing owner")?;
    let name = parts.next().context("slug missing name (expected owner/name)")?;
    Ok((owner, name))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> Client {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        conn.execute_batch(include_str!("../../src/schema.sql")).unwrap();
        Client::from_conn(conn)
    }

    #[test]
    fn prs_empty() {
        let client = setup();
        let prs = client.prs(&PrFilter::default()).unwrap();
        assert!(prs.is_empty());
    }

    #[test]
    fn notifications_empty() {
        let client = setup();
        let notifs = client.unread_notifications().unwrap();
        assert!(notifs.is_empty());
    }

    #[test]
    fn split_slug_valid() {
        assert_eq!(split_slug("myorg/backend").unwrap(), ("myorg", "backend"));
    }

    #[test]
    fn split_slug_missing_name() {
        assert!(split_slug("noname").is_err());
    }

    #[test]
    fn esc_quotes() {
        assert_eq!(esc("O'Brien"), "O''Brien");
    }
}
