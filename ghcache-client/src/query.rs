use anyhow::{Context, Result};
use sqlx::{Row, SqlitePool, sqlite::SqliteConnectOptions};
use std::path::Path;

use crate::types::*;
use crate::tail::ChangeEvent;

pub struct Client {
    pool: SqlitePool,
}

impl Client {
    pub async fn open(db_path: &Path) -> Result<Self> {
        let opts = SqliteConnectOptions::new()
            .filename(db_path)
            .read_only(true);
        let pool = SqlitePool::connect_with(opts).await
            .with_context(|| format!("opening ghcache db at {}", db_path.display()))?;
        Ok(Client { pool })
    }

    pub fn from_pool(pool: SqlitePool) -> Self {
        Client { pool }
    }

    // ---- Pull Requests --------------------------------------------------

    pub async fn prs(&self, filter: &PrFilter<'_>) -> Result<Vec<PullRequest>> {
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
        let sql = format!(
            "SELECT pr.id, r.owner || '/' || r.name, pr.number, pr.title, pr.author,
                    pr.head_ref, pr.base_ref, pr.state, pr.draft, pr.mergeable,
                    pr.additions, pr.deletions, pr.changed_files,
                    pr.created_at, pr.updated_at, pr.merged_at, pr.body
             FROM pull_request pr
             JOIN repo r ON r.id = pr.repo_id
             WHERE {}
             ORDER BY pr.updated_at DESC",
            conditions.join(" AND ")
        );
        let rows = sqlx::query(&sql).fetch_all(&self.pool).await?;
        rows.iter().map(|r| Ok(PullRequest {
            id:            r.try_get(0)?,
            repo_slug:     r.try_get(1)?,
            number:        r.try_get(2)?,
            title:         r.try_get(3)?,
            author:        r.try_get(4)?,
            head_ref:      r.try_get(5)?,
            base_ref:      r.try_get(6)?,
            state:         r.try_get(7)?,
            draft:         r.try_get::<bool, _>(8)?,
            mergeable:     r.try_get(9)?,
            additions:     r.try_get(10)?,
            deletions:     r.try_get(11)?,
            changed_files: r.try_get(12)?,
            created_at:    r.try_get(13)?,
            updated_at:    r.try_get(14)?,
            merged_at:     r.try_get(15)?,
            body:          r.try_get(16)?,
        })).collect::<Result<Vec<_>, sqlx::Error>>().map_err(Into::into)
    }

    pub async fn pr(&self, repo: &str, number: i64) -> Result<Option<PullRequest>> {
        let (owner, name) = split_slug(repo)?;
        let row = sqlx::query(
            "SELECT pr.id, r.owner || '/' || r.name, pr.number, pr.title, pr.author,
                    pr.head_ref, pr.base_ref, pr.state, pr.draft, pr.mergeable,
                    pr.additions, pr.deletions, pr.changed_files,
                    pr.created_at, pr.updated_at, pr.merged_at, pr.body
             FROM pull_request pr
             JOIN repo r ON r.id = pr.repo_id
             WHERE r.owner = ? AND r.name = ? AND pr.number = ?",
        )
        .bind(owner)
        .bind(name)
        .bind(number)
        .fetch_optional(&self.pool)
        .await?;
        row.map(|r| Ok(PullRequest {
            id:            r.try_get(0)?,
            repo_slug:     r.try_get(1)?,
            number:        r.try_get(2)?,
            title:         r.try_get(3)?,
            author:        r.try_get(4)?,
            head_ref:      r.try_get(5)?,
            base_ref:      r.try_get(6)?,
            state:         r.try_get(7)?,
            draft:         r.try_get::<bool, _>(8)?,
            mergeable:     r.try_get(9)?,
            additions:     r.try_get(10)?,
            deletions:     r.try_get(11)?,
            changed_files: r.try_get(12)?,
            created_at:    r.try_get(13)?,
            updated_at:    r.try_get(14)?,
            merged_at:     r.try_get(15)?,
            body:          r.try_get(16)?,
        })).transpose().map_err(Into::into)
    }

    pub async fn reviews(&self, pr_id: i64) -> Result<Vec<Review>> {
        let rows = sqlx::query(
            "SELECT id, pr_id, author, state, body, submitted_at
             FROM pr_review WHERE pr_id = ? ORDER BY submitted_at",
        )
        .bind(pr_id)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(|r| Ok(Review {
            id:           r.try_get(0)?,
            pr_id:        r.try_get(1)?,
            author:       r.try_get(2)?,
            state:        r.try_get(3)?,
            body:         r.try_get(4)?,
            submitted_at: r.try_get(5)?,
        })).collect::<Result<Vec<_>, sqlx::Error>>().map_err(Into::into)
    }

    pub async fn comments(&self, pr_id: i64) -> Result<Vec<Comment>> {
        let rows = sqlx::query(
            "SELECT id, pr_id, author, body, path, line, created_at
             FROM pr_comment WHERE pr_id = ? ORDER BY created_at",
        )
        .bind(pr_id)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(|r| Ok(Comment {
            id:         r.try_get(0)?,
            pr_id:      r.try_get(1)?,
            author:     r.try_get(2)?,
            body:       r.try_get(3)?,
            path:       r.try_get(4)?,
            line:       r.try_get(5)?,
            created_at: r.try_get(6)?,
        })).collect::<Result<Vec<_>, sqlx::Error>>().map_err(Into::into)
    }

    // ---- Notifications --------------------------------------------------

    pub async fn unread_notifications(&self) -> Result<Vec<Notification>> {
        self.notifications(true).await
    }

    pub async fn notifications(&self, unread_only: bool) -> Result<Vec<Notification>> {
        let filter = if unread_only { "WHERE n.unread = 1" } else { "" };
        let sql = format!(
            "SELECT n.id, n.gh_id, r.owner || '/' || r.name, n.subject_type,
                    n.subject_title, n.subject_url, n.subject_number, n.html_url,
                    n.reason, n.unread, n.updated_at
             FROM notification n
             LEFT JOIN repo r ON r.id = n.repo_id
             {filter}
             ORDER BY n.updated_at DESC"
        );
        let rows = sqlx::query(&sql).fetch_all(&self.pool).await?;
        rows.iter().map(|r| Ok(Notification {
            id:             r.try_get(0)?,
            gh_id:          r.try_get(1)?,
            repo_slug:      r.try_get(2)?,
            subject_type:   r.try_get(3)?,
            subject_title:  r.try_get(4)?,
            subject_url:    r.try_get(5)?,
            subject_number: r.try_get(6)?,
            html_url:       r.try_get(7)?,
            reason:         r.try_get(8)?,
            unread:         r.try_get::<bool, _>(9)?,
            updated_at:     r.try_get(10)?,
        })).collect::<Result<Vec<_>, sqlx::Error>>().map_err(Into::into)
    }

    // ---- Branches -------------------------------------------------------

    pub async fn branches(&self, repo: Option<&str>) -> Result<Vec<Branch>> {
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
        let rows = sqlx::query(&sql).fetch_all(&self.pool).await?;
        rows.iter().map(|r| Ok(Branch {
            id:             r.try_get(0)?,
            repo_slug:      r.try_get(1)?,
            name:           r.try_get(2)?,
            sha:            r.try_get(3)?,
            behind_default: r.try_get(4)?,
            ahead_default:  r.try_get(5)?,
            updated_at:     r.try_get(6)?,
        })).collect::<Result<Vec<_>, sqlx::Error>>().map_err(Into::into)
    }

    // ---- Events ---------------------------------------------------------

    pub async fn events(
        &self,
        repo: Option<&str>,
        event_type: Option<&str>,
        limit: usize,
    ) -> Result<Vec<RepoEvent>> {
        let mut conditions = vec!["1=1".to_owned()];
        if let Some(r) = repo {
            let (owner, name) = split_slug(r)?;
            conditions.push(format!("r.owner = '{}' AND r.name = '{}'", esc(owner), esc(name)));
        }
        if let Some(t) = event_type {
            conditions.push(format!("e.type = '{}'", esc(t)));
        }
        let sql = format!(
            "SELECT e.id, r.owner || '/' || r.name, e.gh_id, e.type, e.actor,
                    e.payload_json, e.created_at
             FROM repo_event e JOIN repo r ON r.id = e.repo_id
             WHERE {}
             ORDER BY e.created_at DESC LIMIT {}",
            conditions.join(" AND "),
            limit
        );
        let rows = sqlx::query(&sql).fetch_all(&self.pool).await?;
        rows.iter().map(|r| {
            let payload_str: Option<String> = r.try_get(5)?;
            Ok(RepoEvent {
                id:         r.try_get(0)?,
                repo_slug:  r.try_get(1)?,
                gh_id:      r.try_get(2)?,
                event_type: r.try_get(3)?,
                actor:      r.try_get(4)?,
                payload:    payload_str
                    .as_deref()
                    .and_then(|s| serde_json::from_str(s).ok())
                    .unwrap_or(serde_json::Value::Null),
                created_at: r.try_get(6)?,
            })
        }).collect::<Result<Vec<_>, sqlx::Error>>().map_err(Into::into)
    }

    // ---- Rate limit -----------------------------------------------------

    pub async fn rate_limit(&self) -> Result<Vec<RateLimit>> {
        let rows = sqlx::query(
            "SELECT api_type,
                    rate_remaining,
                    datetime(rate_reset, 'unixepoch') AS reset_at,
                    gql_cost
             FROM call_log
             WHERE id IN (
                 SELECT MAX(id) FROM call_log WHERE rate_remaining IS NOT NULL GROUP BY api_type
             )",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(|r| Ok(RateLimit {
            api_type:  r.try_get(0)?,
            remaining: r.try_get(1)?,
            reset_at:  r.try_get(2)?,
            gql_cost:  r.try_get(3)?,
        })).collect::<Result<Vec<_>, sqlx::Error>>().map_err(Into::into)
    }

    // ---- Change log (prefer Subscriber or EventStream for live push) ----

    pub async fn changes_since(&self, last_id: i64) -> Result<Vec<ChangeEvent>> {
        crate::tail::poll(&self.pool, last_id).await
    }

    pub async fn latest_change_id(&self) -> Result<i64> {
        sqlx::query_scalar("SELECT COALESCE(MAX(id), 0) FROM change_log")
            .fetch_one(&self.pool)
            .await
            .map_err(Into::into)
    }
}

fn esc(s: &str) -> String {
    s.replace('\'', "''")
}

fn split_slug(slug: &str) -> Result<(&str, &str)> {
    let mut parts = slug.splitn(2, '/');
    let owner = parts.next().context("slug missing owner")?;
    let name  = parts.next().context("slug missing name (expected owner/name)")?;
    Ok((owner, name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;

    async fn setup() -> Client {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::raw_sql(include_str!("../../src/schema.sql"))
            .execute(&pool)
            .await
            .unwrap();
        Client::from_pool(pool)
    }

    #[tokio::test]
    async fn prs_empty() {
        let client = setup().await;
        let prs = client.prs(&PrFilter::default()).await.unwrap();
        assert!(prs.is_empty());
    }

    #[tokio::test]
    async fn notifications_empty() {
        let client = setup().await;
        let notifs = client.unread_notifications().await.unwrap();
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
