use anyhow::{Context, Result};
use sqlx::{Row, SqliteConnection, SqlitePool, sqlite::SqliteConnectOptions};
#[cfg(test)]
use sqlx::sqlite::SqlitePoolOptions;
use std::path::Path;

pub async fn open(path: &Path) -> Result<SqlitePool> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating db directory {}", parent.display()))?;
        }
    }
    let opts = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        .pragma("journal_mode", "WAL")
        .pragma("foreign_keys", "ON")
        .pragma("synchronous", "NORMAL")
        .pragma("cache_size", "-8000");
    let pool = SqlitePool::connect_with(opts)
        .await
        .with_context(|| format!("opening database at {}", path.display()))?;
    migrate(&pool).await?;
    Ok(pool)
}

#[cfg(test)]
pub async fn open_in_memory() -> Result<SqlitePool> {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            SqliteConnectOptions::new()
                .filename(":memory:")
                .pragma("foreign_keys", "ON"),
        )
        .await?;
    migrate(&pool).await?;
    Ok(pool)
}

async fn migrate(pool: &SqlitePool) -> Result<()> {
    sqlx::raw_sql(SCHEMA).execute(pool).await?;

    for stmt in [
        "ALTER TABLE notification ADD COLUMN subject_number INTEGER",
        "ALTER TABLE notification ADD COLUMN html_url TEXT",
    ] {
        if let Err(e) = sqlx::query(stmt).execute(pool).await {
            if !e.to_string().contains("duplicate column") {
                return Err(e.into());
            }
        }
    }

    sqlx::raw_sql(
        "DROP VIEW IF EXISTS v_unread_notifications;
         CREATE VIEW v_unread_notifications AS
         SELECT
             n.gh_id, n.subject_type, n.subject_title, n.subject_number,
             n.html_url, n.reason, n.unread, n.updated_at,
             r.owner || '/' || r.name AS repo_slug
         FROM notification n
         LEFT JOIN repo r ON r.id = n.repo_id
         WHERE n.unread = 1
         ORDER BY n.updated_at DESC;",
    )
    .execute(pool)
    .await?;

    Ok(())
}

const SCHEMA: &str = include_str!("schema.sql");

// ---- repo upsert -------------------------------------------------------

pub async fn upsert_repo(
    conn: &mut SqliteConnection,
    owner: &str,
    name: &str,
    default_branch: &str,
) -> Result<i64> {
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO repo (owner, name, default_branch)
         VALUES (?, ?, ?)
         ON CONFLICT(owner, name) DO UPDATE SET
             default_branch = excluded.default_branch
         RETURNING id",
    )
    .bind(owner)
    .bind(name)
    .bind(default_branch)
    .fetch_one(conn)
    .await?;
    Ok(id)
}

pub async fn get_repo_id(
    conn: &mut SqliteConnection,
    owner: &str,
    name: &str,
) -> Result<Option<i64>> {
    let id = sqlx::query_scalar("SELECT id FROM repo WHERE owner = ? AND name = ?")
        .bind(owner)
        .bind(name)
        .fetch_optional(conn)
        .await?;
    Ok(id)
}

pub async fn get_repo_default_branch(
    conn: &mut SqliteConnection,
    owner: &str,
    name: &str,
) -> Result<Option<String>> {
    let branch = sqlx::query_scalar(
        "SELECT default_branch FROM repo WHERE owner = ? AND name = ?"
    )
    .bind(owner)
    .bind(name)
    .fetch_optional(conn)
    .await?;
    Ok(branch)
}

// ---- poll_state --------------------------------------------------------

pub struct PollState {
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub poll_interval: Option<i64>,
    pub last_polled_at: Option<String>,
}

pub async fn get_poll_state(conn: &mut SqliteConnection, endpoint: &str) -> Result<PollState> {
    let row = sqlx::query(
        "SELECT etag, last_modified, poll_interval, last_polled_at
         FROM poll_state WHERE endpoint = ?",
    )
    .bind(endpoint)
    .fetch_optional(conn)
    .await?;

    if let Some(r) = row {
        Ok(PollState {
            etag:           r.try_get(0)?,
            last_modified:  r.try_get(1)?,
            poll_interval:  r.try_get(2)?,
            last_polled_at: r.try_get(3)?,
        })
    } else {
        Ok(PollState { etag: None, last_modified: None, poll_interval: None, last_polled_at: None })
    }
}

pub async fn set_poll_state(
    conn: &mut SqliteConnection,
    endpoint: &str,
    etag: Option<&str>,
    last_modified: Option<&str>,
    poll_interval: Option<i64>,
    changed: bool,
) -> Result<()> {
    let now = chrono::Utc::now().to_rfc3339();
    let changed_at: Option<&str> = if changed { Some(now.as_str()) } else { None };
    sqlx::query(
        "INSERT INTO poll_state (endpoint, etag, last_modified, poll_interval, last_polled_at, last_changed_at)
         VALUES (?, ?, ?, ?, ?, ?)
         ON CONFLICT(endpoint) DO UPDATE SET
             etag            = excluded.etag,
             last_modified   = excluded.last_modified,
             poll_interval   = COALESCE(excluded.poll_interval, poll_state.poll_interval),
             last_polled_at  = excluded.last_polled_at,
             last_changed_at = COALESCE(excluded.last_changed_at, poll_state.last_changed_at)",
    )
    .bind(endpoint)
    .bind(etag)
    .bind(last_modified)
    .bind(poll_interval)
    .bind(&now)
    .bind(changed_at)
    .execute(conn)
    .await?;
    Ok(())
}

// ---- call_log ----------------------------------------------------------

pub struct CallLogEntry<'a> {
    pub endpoint:       &'a str,
    pub api_type:       &'a str,
    pub method:         &'a str,
    pub status_code:    Option<u16>,
    pub etag:           Option<&'a str>,
    pub last_modified:  Option<&'a str>,
    pub rate_remaining: Option<i64>,
    pub rate_reset:     Option<i64>,
    pub gql_cost:       Option<i64>,
    pub cache_hit:      bool,
    pub duration_ms:    Option<i64>,
}

pub async fn log_call(conn: &mut SqliteConnection, entry: &CallLogEntry<'_>) -> Result<()> {
    sqlx::query(
        "INSERT INTO call_log
         (endpoint, api_type, method, status_code, etag, last_modified,
          rate_remaining, rate_reset, gql_cost, cache_hit, duration_ms)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(entry.endpoint)
    .bind(entry.api_type)
    .bind(entry.method)
    .bind(entry.status_code.map(|c| c as i64))
    .bind(entry.etag)
    .bind(entry.last_modified)
    .bind(entry.rate_remaining)
    .bind(entry.rate_reset)
    .bind(entry.gql_cost)
    .bind(entry.cache_hit as i64)
    .bind(entry.duration_ms)
    .execute(conn)
    .await?;
    Ok(())
}

// ---- change_log --------------------------------------------------------

pub enum ChangeEvent {
    Inserted,
    Updated,
}

impl ChangeEvent {
    pub fn as_str(&self) -> &'static str {
        match self {
            ChangeEvent::Inserted => "inserted",
            ChangeEvent::Updated  => "updated",
        }
    }
}

pub async fn log_change(
    conn: &mut SqliteConnection,
    entity_type: &str,
    entity_id: i64,
    event: ChangeEvent,
    repo_slug: Option<&str>,
    payload: Option<&serde_json::Value>,
) -> Result<()> {
    let payload_str = payload.map(|p| serde_json::to_string(p)).transpose()?;
    sqlx::query(
        "INSERT INTO change_log (entity_type, entity_id, event, repo_slug, payload_json)
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(entity_type)
    .bind(entity_id)
    .bind(event.as_str())
    .bind(repo_slug)
    .bind(payload_str)
    .execute(conn)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn schema_creates_without_error() {
        open_in_memory().await.unwrap();
    }

    #[tokio::test]
    async fn upsert_repo_idempotent() {
        let pool = open_in_memory().await.unwrap();
        let mut c = pool.acquire().await.unwrap();
        let id1 = upsert_repo(&mut *c, "myorg", "backend", "main").await.unwrap();
        let id2 = upsert_repo(&mut *c, "myorg", "backend", "main").await.unwrap();
        assert_eq!(id1, id2);
    }

    #[tokio::test]
    async fn upsert_repo_updates_default_branch() {
        let pool = open_in_memory().await.unwrap();
        let mut c = pool.acquire().await.unwrap();
        upsert_repo(&mut *c, "myorg", "backend", "master").await.unwrap();
        upsert_repo(&mut *c, "myorg", "backend", "main").await.unwrap();
        drop(c);
        let branch: String = sqlx::query_scalar(
            "SELECT default_branch FROM repo WHERE owner='myorg' AND name='backend'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(branch, "main");
    }

    #[tokio::test]
    async fn get_repo_id_missing_returns_none() {
        let pool = open_in_memory().await.unwrap();
        let mut c = pool.acquire().await.unwrap();
        assert!(get_repo_id(&mut *c, "x", "y").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn get_repo_id_after_upsert() {
        let pool = open_in_memory().await.unwrap();
        let mut c = pool.acquire().await.unwrap();
        let id = upsert_repo(&mut *c, "o", "n", "main").await.unwrap();
        assert_eq!(get_repo_id(&mut *c, "o", "n").await.unwrap(), Some(id));
    }

    #[tokio::test]
    async fn get_repo_default_branch_roundtrip() {
        let pool = open_in_memory().await.unwrap();
        let mut c = pool.acquire().await.unwrap();
        upsert_repo(&mut *c, "o", "n", "develop").await.unwrap();
        let branch = get_repo_default_branch(&mut *c, "o", "n").await.unwrap();
        assert_eq!(branch, Some("develop".into()));
    }

    #[tokio::test]
    async fn get_repo_default_branch_missing_returns_none() {
        let pool = open_in_memory().await.unwrap();
        let mut c = pool.acquire().await.unwrap();
        assert!(get_repo_default_branch(&mut *c, "x", "y").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn poll_state_roundtrip() {
        let pool = open_in_memory().await.unwrap();
        let mut c = pool.acquire().await.unwrap();
        set_poll_state(&mut *c, "/repos/o/n/events", Some("\"abc\""), None, Some(60), true).await.unwrap();
        let ps = get_poll_state(&mut *c, "/repos/o/n/events").await.unwrap();
        assert_eq!(ps.etag.as_deref(), Some("\"abc\""));
        assert_eq!(ps.poll_interval, Some(60));
    }

    #[tokio::test]
    async fn poll_state_preserves_interval_on_304() {
        let pool = open_in_memory().await.unwrap();
        let mut c = pool.acquire().await.unwrap();
        set_poll_state(&mut *c, "/ep", None, None, Some(120), true).await.unwrap();
        set_poll_state(&mut *c, "/ep", None, None, None, false).await.unwrap();
        let ps = get_poll_state(&mut *c, "/ep").await.unwrap();
        assert_eq!(ps.poll_interval, Some(120));
    }

    #[tokio::test]
    async fn call_log_insert() {
        let pool = open_in_memory().await.unwrap();
        let mut c = pool.acquire().await.unwrap();
        log_call(&mut *c, &CallLogEntry {
            endpoint:       "/repos/o/n/pulls",
            api_type:       "rest",
            method:         "GET",
            status_code:    Some(200),
            etag:           Some("\"xyz\""),
            last_modified:  None,
            rate_remaining: Some(4999),
            rate_reset:     Some(1700000000),
            gql_cost:       None,
            cache_hit:      false,
            duration_ms:    Some(142),
        })
        .await
        .unwrap();
        drop(c);
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM call_log")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn foreign_key_enforced() {
        let pool = open_in_memory().await.unwrap();
        let err = sqlx::query("INSERT INTO branch (repo_id, name) VALUES (999, 'main')")
            .execute(&pool)
            .await;
        assert!(err.is_err());
    }

    #[tokio::test]
    async fn views_exist() {
        let pool = open_in_memory().await.unwrap();
        sqlx::raw_sql(
            "SELECT * FROM v_open_prs LIMIT 1;
             SELECT * FROM v_unread_notifications LIMIT 1;
             SELECT * FROM v_recent_events LIMIT 1;
             SELECT * FROM v_rate_limit LIMIT 1;",
        )
        .execute(&pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn change_log_insert() {
        let pool = open_in_memory().await.unwrap();
        let mut c = pool.acquire().await.unwrap();
        let repo_id = upsert_repo(&mut *c, "o", "n", "main").await.unwrap();
        log_change(&mut *c, "pull_request", repo_id, ChangeEvent::Inserted, Some("o/n"), None).await.unwrap();
        log_change(&mut *c, "pull_request", repo_id, ChangeEvent::Updated,  Some("o/n"), None).await.unwrap();
        drop(c);

        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT entity_type, event FROM change_log ORDER BY id",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(rows, vec![
            ("pull_request".into(), "inserted".into()),
            ("pull_request".into(), "updated".into()),
        ]);
    }

    #[tokio::test]
    async fn change_log_tail_pattern() {
        let pool = open_in_memory().await.unwrap();
        let mut c = pool.acquire().await.unwrap();
        log_change(&mut *c, "notification", 1, ChangeEvent::Inserted, None, None).await.unwrap();
        log_change(&mut *c, "notification", 2, ChangeEvent::Inserted, None, None).await.unwrap();
        log_change(&mut *c, "notification", 3, ChangeEvent::Updated,  None, None).await.unwrap();
        drop(c);

        let ids: Vec<i64> = sqlx::query_scalar(
            "SELECT entity_id FROM change_log WHERE id > 1 ORDER BY id",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(ids, vec![2, 3]);
    }

    #[tokio::test]
    async fn change_log_with_payload() {
        let pool = open_in_memory().await.unwrap();
        let mut c = pool.acquire().await.unwrap();
        let payload = serde_json::json!({ "number": 42, "title": "My PR" });
        log_change(&mut *c, "pull_request", 42, ChangeEvent::Inserted, Some("o/n"), Some(&payload)).await.unwrap();
        drop(c);

        let stored: String = sqlx::query_scalar(
            "SELECT payload_json FROM change_log WHERE entity_id=42",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let v: serde_json::Value = serde_json::from_str(&stored).unwrap();
        assert_eq!(v["number"], 42);
    }
}
