use anyhow::{Context, Result};
use rusqlite::Connection;
use std::io::Write;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Default socket path under XDG_RUNTIME_DIR or /tmp.
pub fn default_socket_path() -> PathBuf {
    if let Ok(runtime) = std::env::var("XDG_RUNTIME_DIR") {
        PathBuf::from(runtime).join("ghcache.sock")
    } else {
        PathBuf::from("/tmp/ghcache.sock")
    }
}

/// Tail `change_log` from `last_id`, return new rows as NDJSON strings.
fn poll_changes(conn: &Connection, last_id: &mut i64) -> Result<Vec<String>> {
    let mut stmt = conn.prepare_cached(
        "SELECT id, entity_type, entity_id, event, repo_slug, payload_json, occurred_at
         FROM change_log WHERE id > ?1 ORDER BY id",
    )?;
    let rows: Vec<(i64, serde_json::Value)> = stmt
        .query_map(rusqlite::params![*last_id], |r| {
            let id: i64 = r.get(0)?;
            Ok((id, rusqlite::types::Value::Null, id))  // placeholder
        })?
        .filter_map(|r| r.ok())
        // re-query properly below
        .map(|_| (0i64, serde_json::Value::Null))
        .collect();
    let _ = rows; // discard -- use a proper approach below

    let mut out = vec![];
    let mut stmt = conn.prepare_cached(
        "SELECT id, entity_type, entity_id, event, repo_slug, payload_json, occurred_at
         FROM change_log WHERE id > ?1 ORDER BY id",
    )?;
    let mut db_rows = stmt.query(rusqlite::params![*last_id])?;
    while let Some(row) = db_rows.next()? {
        let id: i64 = row.get(0)?;
        let entity_type: String = row.get(1)?;
        let entity_id: i64 = row.get(2)?;
        let event: String = row.get(3)?;
        let repo_slug: Option<String> = row.get(4)?;
        let payload_json: Option<String> = row.get(5)?;
        let occurred_at: String = row.get(6)?;

        let payload: serde_json::Value = payload_json
            .as_deref()
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or(serde_json::Value::Null);

        let msg = serde_json::json!({
            "id": id,
            "entity_type": entity_type,
            "entity_id": entity_id,
            "event": event,
            "repo_slug": repo_slug,
            "payload": payload,
            "occurred_at": occurred_at,
        });

        out.push(serde_json::to_string(&msg)?);
        *last_id = id;
    }
    Ok(out)
}

type ClientList = Arc<Mutex<Vec<std::os::unix::net::UnixStream>>>;

pub fn run(conn: &Connection, socket_path: &Path) -> Result<()> {
    // Remove stale socket from previous run
    let _ = std::fs::remove_file(socket_path);

    let listener = UnixListener::bind(socket_path)
        .with_context(|| format!("binding Unix socket at {}", socket_path.display()))?;
    listener.set_nonblocking(true)?;

    tracing::info!(socket = %socket_path.display(), "ghcache serve listening");
    println!("serving on {}", socket_path.display());

    let clients: ClientList = Arc::new(Mutex::new(vec![]));
    let clients_accept = Arc::clone(&clients);

    // Accept thread: adds new clients to the list
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            match stream {
                Ok(s) => {
                    tracing::debug!("new client connected");
                    clients_accept.lock().unwrap().push(s);
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(e) => {
                    tracing::warn!(error = %e, "accept error");
                }
            }
        }
    });

    let mut last_id: i64 = {
        // Start from the current tail so we only broadcast new events
        conn.query_row(
            "SELECT COALESCE(MAX(id), 0) FROM change_log",
            [],
            |r| r.get(0),
        ).unwrap_or(0)
    };

    // Poll loop: check change_log every 500ms, broadcast to all clients
    loop {
        std::thread::sleep(Duration::from_millis(500));

        let lines = match poll_changes(conn, &mut last_id) {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!(error = %e, "polling change_log");
                continue;
            }
        };

        if lines.is_empty() {
            continue;
        }

        let mut clients_guard = clients.lock().unwrap();
        clients_guard.retain_mut(|client| {
            for line in &lines {
                if client.write_all(line.as_bytes()).is_err()
                    || client.write_all(b"\n").is_err()
                {
                    return false; // remove dead client
                }
            }
            true
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;

    #[test]
    fn default_socket_path_is_absolute() {
        let p = default_socket_path();
        assert!(p.is_absolute());
        assert!(p.to_str().unwrap().ends_with(".sock"));
    }

    #[test]
    fn poll_changes_empty() {
        let conn = db::open_in_memory().unwrap();
        let mut last_id = 0i64;
        let lines = poll_changes(&conn, &mut last_id).unwrap();
        assert!(lines.is_empty());
        assert_eq!(last_id, 0);
    }

    #[test]
    fn poll_changes_returns_new_rows() {
        let conn = db::open_in_memory().unwrap();
        db::log_change(&conn, "pull_request", 1, db::ChangeEvent::Inserted, Some("o/n"), None).unwrap();
        db::log_change(&conn, "notification", 2, db::ChangeEvent::Updated, None, None).unwrap();

        let mut last_id = 0i64;
        let lines = poll_changes(&conn, &mut last_id).unwrap();
        assert_eq!(lines.len(), 2);
        assert_eq!(last_id, 2);

        // Calling again with updated last_id returns nothing
        let lines2 = poll_changes(&conn, &mut last_id).unwrap();
        assert!(lines2.is_empty());
    }

    #[test]
    fn poll_changes_advances_last_id() {
        let conn = db::open_in_memory().unwrap();
        db::log_change(&conn, "branch", 10, db::ChangeEvent::Inserted, Some("o/n"), None).unwrap();
        db::log_change(&conn, "branch", 11, db::ChangeEvent::Updated, Some("o/n"), None).unwrap();

        let mut last_id = 0i64;
        poll_changes(&conn, &mut last_id).unwrap();
        assert_eq!(last_id, 2); // row id 2

        // Add one more
        db::log_change(&conn, "branch", 12, db::ChangeEvent::Inserted, None, None).unwrap();
        let lines = poll_changes(&conn, &mut last_id).unwrap();
        assert_eq!(lines.len(), 1);

        let v: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(v["entity_id"], 12);
        assert_eq!(v["event"], "inserted");
    }

    #[test]
    fn poll_changes_ndjson_shape() {
        let conn = db::open_in_memory().unwrap();
        let payload = serde_json::json!({"number": 5});
        db::log_change(&conn, "pull_request", 5, db::ChangeEvent::Updated, Some("myorg/backend"), Some(&payload)).unwrap();

        let mut last_id = 0i64;
        let lines = poll_changes(&conn, &mut last_id).unwrap();
        assert_eq!(lines.len(), 1);

        let v: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(v["entity_type"], "pull_request");
        assert_eq!(v["event"], "updated");
        assert_eq!(v["repo_slug"], "myorg/backend");
        assert_eq!(v["payload"]["number"], 5);
        assert!(v["occurred_at"].as_str().is_some());
    }
}
