use anyhow::Result;
use sqlx::SqliteConnection;
use std::collections::HashMap;

use crate::db::{self, ChangeEvent};
use crate::gh::{GitHubClient, GhRequest};

/// Sync repo events. Returns PR numbers touched by PullRequestEvent/PullRequestReviewEvent.
pub async fn sync(
    conn: &mut SqliteConnection,
    gh: &dyn GitHubClient,
    repo_id: i64,
    owner: &str,
    name: &str,
) -> Result<Vec<i64>> {
    let endpoint = format!("/repos/{owner}/{name}/events");
    let poll = db::get_poll_state(conn, &endpoint).await?;

    if let (Some(interval), Some(ref last_polled)) = (poll.poll_interval, &poll.last_polled_at) {
        if let Ok(last) = chrono::DateTime::parse_from_rfc3339(last_polled) {
            let elapsed = chrono::Utc::now().signed_duration_since(last).num_seconds();
            if elapsed < interval {
                tracing::debug!(repo = %format!("{owner}/{name}"), elapsed, interval, "events: skipping, poll interval not elapsed");
                return Ok(vec![]);
            }
        }
    }

    let mut req = GhRequest::get(&endpoint).paginated();
    if let Some(ref etag) = poll.etag {
        req = req.with_etag(etag);
    }

    let resp = gh.call(conn, &req).await?;

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
        let ev_type    = ev["type"].as_str().unwrap_or("");
        let payload    = ev.get("payload").unwrap_or(&serde_json::Value::Null);
        let payload_str = serde_json::to_string(payload)?;

        let rows_affected = sqlx::query(
            "INSERT OR IGNORE INTO repo_event (repo_id, gh_id, type, actor, payload_json, created_at)
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(repo_id)
        .bind(gh_id)
        .bind(ev_type)
        .bind(ev["actor"]["login"].as_str())
        .bind(&payload_str)
        .bind(ev["created_at"].as_str().unwrap_or(""))
        .execute(&mut *conn)
        .await?
        .rows_affected();

        if rows_affected > 0 {
            let row_id: i64 = sqlx::query_scalar("SELECT last_insert_rowid()")
                .fetch_one(&mut *conn)
                .await?;
            db::log_change(conn, "repo_event", row_id, ChangeEvent::Inserted, Some(&slug), None).await?;

            if let Some(pr_num) = pr_number_from_event(ev_type, payload) {
                dirty_prs.push(pr_num);
            }
        }
        inserted += rows_affected as usize;
    }

    tracing::info!(repo = %slug, inserted, total = events.len(), dirty_prs = dirty_prs.len(), "events synced");
    Ok(dirty_prs)
}

/// Sync all events for a GitHub org in one call. Returns repo name → dirty PR numbers.
pub async fn sync_org(
    conn: &mut SqliteConnection,
    gh: &dyn GitHubClient,
    owner: &str,
) -> Result<HashMap<String, Vec<i64>>> {
    let endpoint = format!("/orgs/{owner}/events");
    let poll = db::get_poll_state(conn, &endpoint).await?;

    if let (Some(interval), Some(ref last_polled)) = (poll.poll_interval, &poll.last_polled_at) {
        if let Ok(last) = chrono::DateTime::parse_from_rfc3339(last_polled) {
            let elapsed = chrono::Utc::now().signed_duration_since(last).num_seconds();
            if elapsed < interval {
                tracing::debug!(owner, elapsed, interval, "org events: skipping, poll interval not elapsed");
                return Ok(HashMap::new());
            }
        }
    }

    let mut req = GhRequest::get(&endpoint).paginated();
    if let Some(ref etag) = poll.etag {
        req = req.with_etag(etag);
    }

    let resp = gh.call(conn, &req).await?;

    if resp.is_not_modified() {
        tracing::debug!(owner, "org events: 304 not modified");
        return Ok(HashMap::new());
    }

    let events = match resp.body.as_array() {
        Some(a) => a.clone(),
        None => return Ok(HashMap::new()),
    };

    let mut dirty: HashMap<String, Vec<i64>> = HashMap::new();
    let mut repo_id_cache: HashMap<String, Option<i64>> = HashMap::new();
    let mut inserted_total = 0usize;

    for ev in &events {
        let full_slug = match ev["repo"]["name"].as_str() {
            Some(s) => s,
            None => continue,
        };
        let repo_name = match full_slug.split_once('/') {
            Some((_, n)) => n,
            None => continue,
        };
        let gh_id = match ev["id"].as_str() {
            Some(id) => id,
            None => continue,
        };
        let ev_type = ev["type"].as_str().unwrap_or("");
        let payload = ev.get("payload").unwrap_or(&serde_json::Value::Null);

        let repo_id = match repo_id_cache.get(repo_name) {
            Some(v) => *v,
            None => {
                let id = db::get_repo_id(conn, owner, repo_name).await.ok().flatten();
                repo_id_cache.insert(repo_name.to_string(), id);
                id
            }
        };

        if let Some(repo_id) = repo_id {
            let payload_str = serde_json::to_string(payload).unwrap_or_default();
            let rows_affected = sqlx::query(
                "INSERT OR IGNORE INTO repo_event (repo_id, gh_id, type, actor, payload_json, created_at)
                 VALUES (?, ?, ?, ?, ?, ?)",
            )
            .bind(repo_id)
            .bind(gh_id)
            .bind(ev_type)
            .bind(ev["actor"]["login"].as_str())
            .bind(&payload_str)
            .bind(ev["created_at"].as_str().unwrap_or(""))
            .execute(&mut *conn)
            .await?
            .rows_affected();

            if rows_affected > 0 {
                inserted_total += 1;
                let row_id: i64 = sqlx::query_scalar("SELECT last_insert_rowid()")
                    .fetch_one(&mut *conn)
                    .await?;
                db::log_change(conn, "repo_event", row_id, ChangeEvent::Inserted, Some(full_slug), None).await?;
                if let Some(pr_num) = pr_number_from_event(ev_type, payload) {
                    dirty.entry(repo_name.to_string()).or_default().push(pr_num);
                }
            }
        }
    }

    let dirty_count: usize = dirty.values().map(|v| v.len()).sum();
    tracing::info!(owner, inserted = inserted_total, total = events.len(), dirty_prs = dirty_count, "org events synced");
    Ok(dirty)
}

fn pr_number_from_event(ev_type: &str, payload: &serde_json::Value) -> Option<i64> {
    match ev_type {
        "PullRequestEvent"       => payload["number"].as_i64(),
        "PullRequestReviewEvent" => payload["pull_request"]["number"].as_i64(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use crate::db;
    use sqlx::SqliteConnection;

    fn make_event(id: &str, event_type: &str) -> serde_json::Value {
        serde_json::json!({
            "id": id,
            "type": event_type,
            "actor": { "login": "alice" },
            "payload": { "action": "opened" },
            "created_at": "2026-01-01T00:00:00Z"
        })
    }

    async fn insert_events(conn: &mut SqliteConnection, repo_id: i64, events: &[serde_json::Value]) {
        for ev in events {
            let gh_id   = ev["id"].as_str().unwrap();
            let payload = serde_json::to_string(&ev["payload"]).unwrap();
            sqlx::query(
                "INSERT OR IGNORE INTO repo_event (repo_id, gh_id, type, actor, payload_json, created_at)
                 VALUES (?, ?, ?, ?, ?, ?)",
            )
            .bind(repo_id)
            .bind(gh_id)
            .bind(ev["type"].as_str().unwrap_or(""))
            .bind(ev["actor"]["login"].as_str())
            .bind(&payload)
            .bind(ev["created_at"].as_str().unwrap_or(""))
            .execute(&mut *conn)
            .await
            .unwrap();
        }
    }

    #[tokio::test]
    async fn insert_or_ignore_is_idempotent() {
        let pool = db::open_in_memory().await.unwrap();
        let mut c = pool.acquire().await.unwrap();
        let repo_id = db::upsert_repo(&mut *c, "o", "n", "main").await.unwrap();

        let ev = make_event("100", "PushEvent");
        insert_events(&mut *c, repo_id, &[ev.clone(), ev]).await;
        drop(c);

        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM repo_event")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn multiple_events_inserted() {
        let pool = db::open_in_memory().await.unwrap();
        let mut c = pool.acquire().await.unwrap();
        let repo_id = db::upsert_repo(&mut *c, "o", "n", "main").await.unwrap();

        let events = vec![
            make_event("1", "PushEvent"),
            make_event("2", "PullRequestEvent"),
            make_event("3", "IssuesEvent"),
        ];
        insert_events(&mut *c, repo_id, &events).await;
        drop(c);

        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM repo_event")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 3);
    }

    #[tokio::test]
    async fn v_recent_events_view() {
        let pool = db::open_in_memory().await.unwrap();
        let mut c = pool.acquire().await.unwrap();
        let repo_id = db::upsert_repo(&mut *c, "o", "n", "main").await.unwrap();

        let mut ev = make_event("1", "PullRequestEvent");
        ev["payload"] = serde_json::json!({ "action": "opened", "pull_request": { "title": "My PR" } });
        insert_events(&mut *c, repo_id, &[ev]).await;
        drop(c);

        let subject: Option<String> = sqlx::query_scalar("SELECT subject FROM v_recent_events LIMIT 1")
            .fetch_optional(&pool)
            .await
            .unwrap();
        assert_eq!(subject.as_deref(), Some("My PR"));
    }
}
