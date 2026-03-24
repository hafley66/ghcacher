use anyhow::Result;
use rusqlite::{Connection, params};

use crate::db;
use crate::gh::{GhClient, GhRequest};

pub fn sync(conn: &Connection, gh: &GhClient, repo_id: i64, owner: &str, name: &str) -> Result<()> {
    let endpoint = format!("/repos/{owner}/{name}/events");
    let poll = db::get_poll_state(conn, &endpoint)?;

    let mut req = GhRequest::get(&endpoint).paginated();
    if let Some(ref etag) = poll.etag {
        req = req.with_etag(etag);
    }

    let resp = gh.call(conn, &req)?;

    if resp.is_not_modified() {
        tracing::debug!(repo = %format!("{owner}/{name}"), "events: 304 not modified");
        return Ok(());
    }

    let events = match resp.body.as_array() {
        Some(a) => a,
        None => return Ok(()),
    };

    let mut inserted = 0usize;
    for ev in events {
        let gh_id = match ev["id"].as_str() {
            Some(id) => id,
            None => continue,
        };
        let payload = serde_json::to_string(ev.get("payload").unwrap_or(&serde_json::Value::Null))?;

        let n = conn.execute(
            "INSERT OR IGNORE INTO repo_event (repo_id, gh_id, type, actor, payload_json, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                repo_id,
                gh_id,
                ev["type"].as_str().unwrap_or(""),
                ev["actor"]["login"].as_str(),
                payload,
                ev["created_at"].as_str().unwrap_or(""),
            ],
        )?;
        inserted += n;
    }

    tracing::info!(repo = %format!("{owner}/{name}"), inserted, total = events.len(), "events synced");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;

    fn make_event(id: &str, event_type: &str) -> serde_json::Value {
        serde_json::json!({
            "id": id,
            "type": event_type,
            "actor": { "login": "alice" },
            "payload": { "action": "opened" },
            "created_at": "2026-01-01T00:00:00Z"
        })
    }

    fn insert_events(conn: &Connection, repo_id: i64, events: &[serde_json::Value]) -> Result<()> {
        for ev in events {
            let gh_id = ev["id"].as_str().unwrap();
            let payload = serde_json::to_string(&ev["payload"]).unwrap();
            conn.execute(
                "INSERT OR IGNORE INTO repo_event (repo_id, gh_id, type, actor, payload_json, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    repo_id,
                    gh_id,
                    ev["type"].as_str().unwrap_or(""),
                    ev["actor"]["login"].as_str(),
                    payload,
                    ev["created_at"].as_str().unwrap_or(""),
                ],
            )?;
        }
        Ok(())
    }

    #[test]
    fn insert_or_ignore_is_idempotent() {
        let conn = db::open_in_memory().unwrap();
        let repo_id = db::upsert_repo(&conn, "o", "n", "main").unwrap();

        let ev = make_event("100", "PushEvent");
        insert_events(&conn, repo_id, &[ev.clone(), ev]).unwrap();

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM repo_event", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn multiple_events_inserted() {
        let conn = db::open_in_memory().unwrap();
        let repo_id = db::upsert_repo(&conn, "o", "n", "main").unwrap();

        let events = vec![
            make_event("1", "PushEvent"),
            make_event("2", "PullRequestEvent"),
            make_event("3", "IssuesEvent"),
        ];
        insert_events(&conn, repo_id, &events).unwrap();

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM repo_event", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 3);
    }

    #[test]
    fn v_recent_events_view() {
        let conn = db::open_in_memory().unwrap();
        let repo_id = db::upsert_repo(&conn, "o", "n", "main").unwrap();

        let mut ev = make_event("1", "PullRequestEvent");
        ev["payload"] = serde_json::json!({ "action": "opened", "pull_request": { "title": "My PR" } });
        insert_events(&conn, repo_id, &[ev]).unwrap();

        let subject: Option<String> = conn
            .query_row("SELECT subject FROM v_recent_events LIMIT 1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(subject.as_deref(), Some("My PR"));
    }
}
