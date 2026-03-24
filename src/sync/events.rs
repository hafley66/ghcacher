use anyhow::Result;
use rusqlite::{Connection, params};

use crate::db::{self, ChangeEvent};
use crate::gh::{GitHubClient, GhRequest};

/// Sync repo events. Returns PR numbers that were touched by newly seen
/// PullRequestEvent or PullRequestReviewEvent entries -- callers use this
/// to drive targeted PR syncs instead of full sweeps.
pub fn sync(conn: &Connection, gh: &dyn GitHubClient, repo_id: i64, owner: &str, name: &str) -> Result<Vec<i64>> {
    let endpoint = format!("/repos/{owner}/{name}/events");
    let poll = db::get_poll_state(conn, &endpoint)?;

    let mut req = GhRequest::get(&endpoint).paginated();
    if let Some(ref etag) = poll.etag {
        req = req.with_etag(etag);
    }

    let resp = gh.call(conn, &req)?;

    if resp.is_not_modified() {
        tracing::debug!(repo = %format!("{owner}/{name}"), "events: 304 not modified");
        return Ok(vec![]);
    }

    let events = match resp.body.as_array() {
        Some(a) => a,
        None => return Ok(vec![]),
    };

    let slug = format!("{owner}/{name}");
    let mut inserted = 0usize;
    let mut dirty_prs: Vec<i64> = vec![];

    for ev in events {
        let gh_id = match ev["id"].as_str() {
            Some(id) => id,
            None => continue,
        };
        let ev_type = ev["type"].as_str().unwrap_or("");
        let payload = ev.get("payload").unwrap_or(&serde_json::Value::Null);
        let payload_str = serde_json::to_string(payload)?;

        let n = conn.execute(
            "INSERT OR IGNORE INTO repo_event (repo_id, gh_id, type, actor, payload_json, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                repo_id,
                gh_id,
                ev_type,
                ev["actor"]["login"].as_str(),
                payload_str,
                ev["created_at"].as_str().unwrap_or(""),
            ],
        )?;
        if n > 0 {
            let row_id: i64 = conn.query_row(
                "SELECT id FROM repo_event WHERE repo_id=?1 AND gh_id=?2",
                params![repo_id, gh_id],
                |r| r.get(0),
            )?;
            db::log_change(conn, "repo_event", row_id, ChangeEvent::Inserted, Some(&slug), None)?;

            if let Some(pr_num) = pr_number_from_event(ev_type, payload) {
                dirty_prs.push(pr_num);
            }
        }
        inserted += n;
    }

    tracing::info!(repo = %slug, inserted, total = events.len(), dirty_prs = dirty_prs.len(), "events synced");
    Ok(dirty_prs)
}

fn pr_number_from_event(ev_type: &str, payload: &serde_json::Value) -> Option<i64> {
    match ev_type {
        "PullRequestEvent" => payload["number"].as_i64(),
        "PullRequestReviewEvent" => payload["pull_request"]["number"].as_i64(),
        _ => None,
    }
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
