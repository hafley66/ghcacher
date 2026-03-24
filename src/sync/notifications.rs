use anyhow::Result;
use rusqlite::{Connection, params};

use crate::config::ResolvedConfig;
use crate::db::{self, ChangeEvent};
use crate::gh::{GitHubClient, GhRequest};

const ENDPOINT: &str = "/notifications";

pub fn sync(conn: &Connection, gh: &dyn GitHubClient, cfg: &ResolvedConfig) -> Result<()> {
    let poll = db::get_poll_state(conn, ENDPOINT)?;

    let mut req = GhRequest::get(ENDPOINT);
    if let Some(ref lm) = poll.last_modified {
        req = req.with_last_modified(lm);
    }

    let resp = gh.call(conn, &req)?;

    if resp.is_not_modified() {
        tracing::debug!("notifications: 304 not modified");
        return Ok(());
    }

    let threads = match resp.body.as_array() {
        Some(a) => a,
        None => return Ok(()),
    };

    // Build set of repo slugs we care about
    let tracked_slugs: std::collections::HashSet<String> = cfg
        .repos
        .iter()
        .filter(|r| r.sync_notifications.unwrap_or(false))
        .map(|r| format!("{}/{}", r.owner, r.name))
        .collect();

    let mut upserted = 0usize;
    for thread in threads {
        let repo_slug = thread["repository"]["full_name"].as_str().unwrap_or("");
        if !tracked_slugs.is_empty() && !tracked_slugs.contains(repo_slug) {
            continue;
        }

        upsert_notification(conn, thread)?;
        upserted += 1;
    }

    tracing::info!(upserted, total = threads.len(), "notifications synced");
    Ok(())
}

fn upsert_notification(conn: &Connection, thread: &serde_json::Value) -> Result<()> {
    let gh_id = match thread["id"].as_str() {
        Some(id) => id,
        None => return Ok(()),
    };

    let owner = thread["repository"]["owner"]["login"].as_str().unwrap_or("");
    let name = thread["repository"]["name"].as_str().unwrap_or("");
    let repo_id = db::get_repo_id(conn, owner, name)?;

    conn.execute(
        "INSERT INTO notification
         (gh_id, repo_id, subject_type, subject_title, subject_url, reason, unread, updated_at, last_read_at)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)
         ON CONFLICT(gh_id) DO UPDATE SET
             subject_title = excluded.subject_title,
             subject_url   = excluded.subject_url,
             reason        = excluded.reason,
             unread        = excluded.unread,
             updated_at    = excluded.updated_at,
             last_read_at  = excluded.last_read_at",
        params![
            gh_id,
            repo_id,
            thread["subject"]["type"].as_str().unwrap_or(""),
            thread["subject"]["title"].as_str().unwrap_or(""),
            thread["subject"]["url"].as_str(),
            thread["reason"].as_str().unwrap_or(""),
            thread["unread"].as_bool().unwrap_or(true) as i64,
            thread["updated_at"].as_str().unwrap_or(""),
            thread["last_read_at"].as_str(),
        ],
    )?;

    let notif_id: i64 = conn.query_row(
        "SELECT id FROM notification WHERE gh_id = ?1",
        params![gh_id],
        |r| r.get(0),
    )?;
    let event = if conn.last_insert_rowid() == notif_id {
        ChangeEvent::Inserted
    } else {
        ChangeEvent::Updated
    };
    let owner = thread["repository"]["owner"]["login"].as_str().unwrap_or("");
    let name = thread["repository"]["name"].as_str().unwrap_or("");
    let slug = if owner.is_empty() { None } else { Some(format!("{owner}/{name}")) };
    db::log_change(conn, "notification", notif_id, event, slug.as_deref(), None)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;

    fn make_thread(id: &str, owner: &str, name: &str, unread: bool) -> serde_json::Value {
        serde_json::json!({
            "id": id,
            "repository": {
                "full_name": format!("{owner}/{name}"),
                "owner": { "login": owner },
                "name": name
            },
            "subject": {
                "type": "PullRequest",
                "title": "Some PR",
                "url": "https://api.github.com/repos/o/n/pulls/1"
            },
            "reason": "review_requested",
            "unread": unread,
            "updated_at": "2026-01-01T00:00:00Z",
            "last_read_at": null
        })
    }

    #[test]
    fn upsert_notification_insert() {
        let conn = db::open_in_memory().unwrap();
        let thread = make_thread("thread1", "o", "n", true);
        upsert_notification(&conn, &thread).unwrap();

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM notification", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn upsert_notification_marks_read() {
        let conn = db::open_in_memory().unwrap();

        let thread = make_thread("thread1", "o", "n", true);
        upsert_notification(&conn, &thread).unwrap();

        let mut read = thread.clone();
        read["unread"] = serde_json::json!(false);
        upsert_notification(&conn, &read).unwrap();

        let unread: i64 = conn
            .query_row("SELECT unread FROM notification WHERE gh_id='thread1'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(unread, 0);
    }

    #[test]
    fn upsert_notification_idempotent() {
        let conn = db::open_in_memory().unwrap();
        let thread = make_thread("t1", "o", "n", true);
        upsert_notification(&conn, &thread).unwrap();
        upsert_notification(&conn, &thread).unwrap();

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM notification", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn notification_links_repo_id() {
        let conn = db::open_in_memory().unwrap();
        db::upsert_repo(&conn, "o", "n", "main").unwrap();

        let thread = make_thread("t1", "o", "n", true);
        upsert_notification(&conn, &thread).unwrap();

        let repo_id: Option<i64> = conn
            .query_row("SELECT repo_id FROM notification WHERE gh_id='t1'", [], |r| r.get(0))
            .unwrap();
        assert!(repo_id.is_some());
    }

    #[test]
    fn v_unread_view() {
        let conn = db::open_in_memory().unwrap();
        upsert_notification(&conn, &make_thread("t1", "o", "n", true)).unwrap();
        upsert_notification(&conn, &make_thread("t2", "o", "n", false)).unwrap();

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM v_unread_notifications", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }
}
