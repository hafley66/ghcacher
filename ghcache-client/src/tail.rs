use anyhow::Result;
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChangeEvent {
    pub id: i64,
    pub entity_type: String,
    pub entity_id: i64,
    pub event: String,
    pub repo_slug: Option<String>,
    pub payload: serde_json::Value,
    pub occurred_at: String,
}

/// Poll `change_log` for rows with id > `last_id`. Does not block.
pub fn poll(conn: &Connection, last_id: i64) -> Result<Vec<ChangeEvent>> {
    let mut stmt = conn.prepare_cached(
        "SELECT id, entity_type, entity_id, event, repo_slug, payload_json, occurred_at
         FROM change_log WHERE id > ?1 ORDER BY id",
    )?;
    let rows: Vec<ChangeEvent> = stmt.query_map(params![last_id], |r| {
        let payload_str: Option<String> = r.get(5)?;
        Ok(ChangeEvent {
            id: r.get(0)?,
            entity_type: r.get(1)?,
            entity_id: r.get(2)?,
            event: r.get(3)?,
            repo_slug: r.get(4)?,
            payload: payload_str
                .as_deref()
                .and_then(|s| serde_json::from_str(s).ok())
                .unwrap_or(serde_json::Value::Null),
            occurred_at: r.get(6)?,
        })
    })?
    .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Polls `change_log` on an interval, calling `handler` for each batch of new events.
/// Blocks until `handler` returns `Err` or the process is killed.
///
/// Example:
/// ```no_run
/// ghcache_client::Subscriber::new("/path/to/gh.db")
///     .interval(std::time::Duration::from_millis(500))
///     .subscribe(|events| {
///         for ev in events { println!("{:?}", ev); }
///         Ok(())
///     });
/// ```
pub struct Subscriber {
    db_path: PathBuf,
    interval: Duration,
}

impl Subscriber {
    pub fn new(db_path: impl Into<PathBuf>) -> Self {
        Subscriber {
            db_path: db_path.into(),
            interval: Duration::from_millis(500),
        }
    }

    pub fn interval(mut self, d: Duration) -> Self {
        self.interval = d;
        self
    }

    pub fn subscribe<F>(self, mut handler: F) -> Result<()>
    where
        F: FnMut(Vec<ChangeEvent>) -> Result<()>,
    {
        let conn = Connection::open_with_flags(
            &self.db_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        conn.execute_batch("PRAGMA journal_mode = WAL;")?;

        let mut last_id: i64 = conn
            .query_row("SELECT COALESCE(MAX(id), 0) FROM change_log", [], |r| r.get(0))
            .unwrap_or(0);

        loop {
            std::thread::sleep(self.interval);
            let events = poll(&conn, last_id)?;
            if !events.is_empty() {
                last_id = events.last().map(|e| e.id).unwrap_or(last_id);
                handler(events)?;
            }
        }
    }
}

/// Connects to `GET /events` on the ghcache HTTP server and delivers
/// change events as they arrive. Sends `Last-Event-ID` on connect so the
/// server replays any rows the client missed since its last-seen id.
///
/// Blocks until the connection drops or `handler` returns `Err`.
///
/// Example:
/// ```no_run
/// ghcache_client::EventStream::new(7748)
///     .subscribe(|ev| {
///         println!("{:?}", ev);
///         Ok(())
///     });
/// ```
pub struct EventStream {
    port: u16,
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

    pub fn subscribe<F>(self, mut handler: F) -> Result<()>
    where
        F: FnMut(ChangeEvent) -> Result<()>,
    {
        let stream = TcpStream::connect(format!("127.0.0.1:{}", self.port))
            .map_err(|e| anyhow::anyhow!("connect to ghcache on port {}: {e}", self.port))?;

        {
            use std::io::Write;
            let mut w = &stream;
            let req = format!(
                "GET /events HTTP/1.1\r\nHost: 127.0.0.1\r\nLast-Event-ID: {}\r\nAccept: text/event-stream\r\nConnection: keep-alive\r\n\r\n",
                self.last_id
            );
            w.write_all(req.as_bytes())?;
        }

        let mut reader = BufReader::new(&stream);

        // Discard HTTP response headers.
        loop {
            let mut line = String::new();
            reader.read_line(&mut line)?;
            if line.trim().is_empty() {
                break;
            }
        }

        let mut current_id: Option<i64> = None;
        let mut current_data = String::new();

        loop {
            let mut line = String::new();
            let n = reader.read_line(&mut line)?;
            if n == 0 {
                anyhow::bail!("SSE stream closed by server");
            }
            let trimmed = line.trim_end_matches(['\n', '\r']);

            if let Some(rest) = trimmed.strip_prefix("id:") {
                current_id = rest.trim().parse().ok();
            } else if let Some(rest) = trimmed.strip_prefix("data:") {
                current_data = rest.trim().to_string();
            } else if trimmed.is_empty() && !current_data.is_empty() {
                if let Ok(ev) = serde_json::from_str::<ChangeEvent>(&current_data) {
                    handler(ev)?;
                }
                current_data.clear();
                current_id = None;
            }
            // comment lines (start with ':') and unknown fields are ignored
            let _ = current_id;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn setup() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        conn.execute_batch(include_str!("../../src/schema.sql")).unwrap();
        conn
    }

    fn insert_change(conn: &Connection, entity_type: &str, entity_id: i64) {
        conn.execute(
            "INSERT INTO change_log (entity_type, entity_id, event, repo_slug)
             VALUES (?1, ?2, 'inserted', 'o/n')",
            params![entity_type, entity_id],
        ).unwrap();
    }

    #[test]
    fn poll_empty() {
        let conn = setup();
        let events = poll(&conn, 0).unwrap();
        assert!(events.is_empty());
    }

    #[test]
    fn poll_returns_new() {
        let conn = setup();
        insert_change(&conn, "pull_request", 1);
        insert_change(&conn, "pull_request", 2);

        let events = poll(&conn, 0).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].entity_type, "pull_request");
        assert_eq!(events[1].entity_id, 2);
    }

    #[test]
    fn poll_incremental() {
        let conn = setup();
        insert_change(&conn, "branch", 10);
        insert_change(&conn, "branch", 11);

        let first = poll(&conn, 0).unwrap();
        assert_eq!(first.len(), 2);
        let last_id = first.last().unwrap().id;

        insert_change(&conn, "notification", 99);
        let second = poll(&conn, last_id).unwrap();
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].entity_type, "notification");
    }

    #[test]
    fn poll_event_fields() {
        let conn = setup();
        conn.execute(
            "INSERT INTO change_log (entity_type, entity_id, event, repo_slug, payload_json)
             VALUES ('pull_request', 42, 'updated', 'myorg/backend', '{\"number\":42}')",
            [],
        ).unwrap();

        let events = poll(&conn, 0).unwrap();
        assert_eq!(events.len(), 1);
        let ev = &events[0];
        assert_eq!(ev.event, "updated");
        assert_eq!(ev.repo_slug.as_deref(), Some("myorg/backend"));
        assert_eq!(ev.payload["number"], 42);
    }

    #[test]
    fn change_event_serializes_to_json() {
        let ev = ChangeEvent {
            id: 1,
            entity_type: "pull_request".into(),
            entity_id: 5,
            event: "inserted".into(),
            repo_slug: Some("o/n".into()),
            payload: serde_json::json!({"title": "Fix bug"}),
            occurred_at: "2026-01-01T00:00:00Z".into(),
        };
        let json = serde_json::to_string(&ev).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["entity_type"], "pull_request");
        assert_eq!(v["payload"]["title"], "Fix bug");
    }
}
