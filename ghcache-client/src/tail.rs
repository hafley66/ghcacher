use anyhow::Result;
use serde::{Deserialize, Serialize};
use sqlx::{Row, SqlitePool, sqlite::SqliteConnectOptions, sqlite::SqlitePoolOptions};
use std::path::PathBuf;
use std::time::Duration;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChangeEvent {
    pub id:          i64,
    pub entity_type: String,
    pub entity_id:   i64,
    pub event:       String,
    pub repo_slug:   Option<String>,
    pub payload:     serde_json::Value,
    pub occurred_at: String,
}

/// Poll `change_log` for rows with id > `last_id`. Does not block.
pub async fn poll(pool: &SqlitePool, last_id: i64) -> Result<Vec<ChangeEvent>> {
    let rows = sqlx::query(
        "SELECT id, entity_type, entity_id, event, repo_slug, payload_json, occurred_at
         FROM change_log WHERE id > ? ORDER BY id",
    )
    .bind(last_id)
    .fetch_all(pool)
    .await?;

    rows.iter().map(|r| {
        let payload_str: Option<String> = r.try_get(5)?;
        Ok(ChangeEvent {
            id:          r.try_get(0)?,
            entity_type: r.try_get(1)?,
            entity_id:   r.try_get(2)?,
            event:       r.try_get(3)?,
            repo_slug:   r.try_get(4)?,
            payload:     payload_str
                .as_deref()
                .and_then(|s| serde_json::from_str(s).ok())
                .unwrap_or(serde_json::Value::Null),
            occurred_at: r.try_get(6)?,
        })
    }).collect::<Result<Vec<_>, sqlx::Error>>().map_err(Into::into)
}

/// Polls `change_log` on an interval, calling `handler` for each batch of new events.
/// Blocks (async-blocks) until `handler` returns `Err` or the task is cancelled.
///
/// ```no_run
/// ghcache_client::Subscriber::new("/path/to/gh.db")
///     .interval(std::time::Duration::from_millis(500))
///     .subscribe(|events| async move {
///         for ev in events { println!("{:?}", ev); }
///         Ok(())
///     })
///     .await
///     .unwrap();
/// ```
pub struct Subscriber {
    db_path:  PathBuf,
    interval: Duration,
}

impl Subscriber {
    pub fn new(db_path: impl Into<PathBuf>) -> Self {
        Subscriber { db_path: db_path.into(), interval: Duration::from_millis(500) }
    }

    pub fn interval(mut self, d: Duration) -> Self {
        self.interval = d;
        self
    }

    pub async fn subscribe<F, Fut>(self, mut handler: F) -> Result<()>
    where
        F: FnMut(Vec<ChangeEvent>) -> Fut,
        Fut: std::future::Future<Output = Result<()>>,
    {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(SqliteConnectOptions::new().filename(&self.db_path).read_only(true))
            .await?;

        let mut last_id: i64 = sqlx::query_scalar("SELECT COALESCE(MAX(id), 0) FROM change_log")
            .fetch_one(&pool)
            .await
            .unwrap_or(0);

        loop {
            tokio::time::sleep(self.interval).await;
            let events = poll(&pool, last_id).await?;
            if !events.is_empty() {
                last_id = events.last().map(|e| e.id).unwrap_or(last_id);
                handler(events).await?;
            }
        }
    }
}

/// Connects to `GET /events` on the ghcache HTTP server and delivers
/// change events as they arrive via SSE. Sends `Last-Event-ID` on connect
/// so the server replays any rows missed since last-seen id.
///
/// Runs until the connection closes or `handler` returns `Err`.
///
/// ```no_run
/// ghcache_client::EventStream::new(7748)
///     .subscribe(|ev| async move {
///         println!("{:?}", ev);
///         Ok(())
///     })
///     .await
///     .unwrap();
/// ```
pub struct EventStream {
    port:    u16,
    last_id: i64,
}

impl EventStream {
    pub fn new(port: u16) -> Self {
        EventStream { port, last_id: 0 }
    }

    /// Resume from a known last-seen change_log id (server replays missed rows).
    pub fn from_id(port: u16, last_id: i64) -> Self {
        EventStream { port, last_id }
    }

    pub async fn subscribe<F, Fut>(self, mut handler: F) -> Result<()>
    where
        F: FnMut(ChangeEvent) -> Fut,
        Fut: std::future::Future<Output = Result<()>>,
    {
        let client = reqwest::Client::new();
        let mut response = client
            .get(format!("http://127.0.0.1:{}/events", self.port))
            .header("Last-Event-ID", self.last_id.to_string())
            .header("Accept", "text/event-stream")
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("connect to ghcache SSE on port {}: {e}", self.port))?;

        let mut buffer      = String::new();
        let mut current_data = String::new();

        loop {
            let chunk = match response.chunk().await? {
                Some(c) => c,
                None    => anyhow::bail!("SSE stream closed by server"),
            };

            let text = std::str::from_utf8(&chunk)
                .map_err(|e| anyhow::anyhow!("non-UTF8 SSE chunk: {e}"))?;
            buffer.push_str(text);

            while let Some(idx) = buffer.find('\n') {
                let line = buffer[..idx].trim_end_matches('\r').to_string();
                buffer   = buffer[idx + 1..].to_string();

                if let Some(rest) = line.strip_prefix("data:") {
                    current_data = rest.trim().to_string();
                } else if line.trim().is_empty() && !current_data.is_empty() {
                    if let Ok(ev) = serde_json::from_str::<ChangeEvent>(&current_data) {
                        handler(ev).await?;
                    }
                    current_data.clear();
                }
                // id: lines and SSE comments (:) are ignored
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;

    async fn setup() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::raw_sql(include_str!("../../src/schema.sql"))
            .execute(&pool)
            .await
            .unwrap();
        pool
    }

    async fn insert_change(pool: &SqlitePool, entity_type: &str, entity_id: i64) {
        sqlx::query(
            "INSERT INTO change_log (entity_type, entity_id, event, repo_slug)
             VALUES (?, ?, 'inserted', 'o/n')",
        )
        .bind(entity_type)
        .bind(entity_id)
        .execute(pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn poll_empty() {
        let pool = setup().await;
        let events = poll(&pool, 0).await.unwrap();
        assert!(events.is_empty());
    }

    #[tokio::test]
    async fn poll_returns_new() {
        let pool = setup().await;
        insert_change(&pool, "pull_request", 1).await;
        insert_change(&pool, "pull_request", 2).await;

        let events = poll(&pool, 0).await.unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].entity_type, "pull_request");
        assert_eq!(events[1].entity_id, 2);
    }

    #[tokio::test]
    async fn poll_incremental() {
        let pool = setup().await;
        insert_change(&pool, "branch", 10).await;
        insert_change(&pool, "branch", 11).await;

        let first = poll(&pool, 0).await.unwrap();
        assert_eq!(first.len(), 2);
        let last_id = first.last().unwrap().id;

        insert_change(&pool, "notification", 99).await;
        let second = poll(&pool, last_id).await.unwrap();
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].entity_type, "notification");
    }

    #[tokio::test]
    async fn poll_event_fields() {
        let pool = setup().await;
        sqlx::query(
            "INSERT INTO change_log (entity_type, entity_id, event, repo_slug, payload_json)
             VALUES ('pull_request', 42, 'updated', 'myorg/backend', '{\"number\":42}')",
        )
        .execute(&pool)
        .await
        .unwrap();

        let events = poll(&pool, 0).await.unwrap();
        assert_eq!(events.len(), 1);
        let ev = &events[0];
        assert_eq!(ev.event, "updated");
        assert_eq!(ev.repo_slug.as_deref(), Some("myorg/backend"));
        assert_eq!(ev.payload["number"], 42);
    }

    #[tokio::test]
    async fn change_event_serializes_to_json() {
        let ev = ChangeEvent {
            id:          1,
            entity_type: "pull_request".into(),
            entity_id:   5,
            event:       "inserted".into(),
            repo_slug:   Some("o/n".into()),
            payload:     serde_json::json!({"title": "Fix bug"}),
            occurred_at: "2026-01-01T00:00:00Z".into(),
        };
        let json = serde_json::to_string(&ev).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["entity_type"], "pull_request");
        assert_eq!(v["payload"]["title"], "Fix bug");
    }
}
