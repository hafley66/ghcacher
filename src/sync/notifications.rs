use anyhow::Result;
use rusqlite::{Connection, params};

use crate::config::ResolvedConfig;
use crate::db::{self, ChangeEvent};
use crate::gh::{GitHubClient, GhRequest};

const ENDPOINT: &str = "/notifications";

/// `extra_slugs` are (owner, name) pairs from active subscriptions with
/// `notifications: true` that are not in cfg.repos.
pub fn sync(
    conn: &Connection,
    gh: &dyn GitHubClient,
    cfg: &ResolvedConfig,
    extra_slugs: &[(String, String)],
) -> Result<()> {
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

    // Build set of repo slugs we care about (config + active subscriptions).
    let mut tracked_slugs: std::collections::HashSet<String> = cfg
        .repos
        .iter()
        .filter(|r| r.sync_notifications.unwrap_or(false))
        .map(|r| format!("{}/{}", r.owner, r.name))
        .collect();
    for (owner, name) in extra_slugs {
        tracked_slugs.insert(format!("{owner}/{name}"));
    }

    // Preload repo_id map and existing gh_ids so upsert_notification needs no
    // per-row SELECTs.
    let repo_id_map = load_repo_id_map(conn)?;
    let existing_gh_ids = load_existing_gh_ids(conn)?;

    let mut upserted = 0usize;
    for thread in threads {
        let repo_slug = thread["repository"]["full_name"].as_str().unwrap_or("");
        if !tracked_slugs.is_empty() && !tracked_slugs.contains(repo_slug) {
            continue;
        }

        upsert_notification(conn, thread, &repo_id_map, &existing_gh_ids)?;
        upserted += 1;
    }

    tracing::info!(upserted, total = threads.len(), "notifications synced");
    Ok(())
}

fn load_repo_id_map(conn: &Connection) -> Result<std::collections::HashMap<String, i64>> {
    use std::collections::HashMap;
    let mut stmt = conn.prepare("SELECT owner || '/' || name, id FROM repo")?;
    let mut map = HashMap::new();
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let slug: String = row.get(0)?;
        let id: i64 = row.get(1)?;
        map.insert(slug, id);
    }
    Ok(map)
}

fn load_existing_gh_ids(conn: &Connection) -> Result<std::collections::HashSet<String>> {
    use std::collections::HashSet;
    let mut stmt = conn.prepare("SELECT gh_id FROM notification")?;
    let mut set = HashSet::new();
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let gh_id: String = row.get(0)?;
        set.insert(gh_id);
    }
    Ok(set)
}

/// Converts a GitHub API subject URL to a browser HTML URL and extracts the
/// subject number (PR number, issue number) if present.
///
/// API URL patterns:
///   .../pulls/123   → .../pull/123  (note: plural → singular)
///   .../issues/123  → .../issues/123
///   .../commits/sha → .../commit/sha
fn parse_subject_url(api_url: &str) -> (Option<String>, Option<i64>) {
    // Strip API prefix: https://api.github.com/repos/{owner}/{name}/...
    // to https://github.com/{owner}/{name}/...
    let path = if let Some(rest) = api_url.strip_prefix("https://api.github.com/repos/") {
        rest
    } else {
        return (None, None);
    };

    // path is now like: owner/name/pulls/123  or  owner/name/commits/sha
    let segments: Vec<&str> = path.splitn(4, '/').collect();
    if segments.len() < 4 {
        return (None, None);
    }
    let (owner, repo, kind, tail) = (segments[0], segments[1], segments[2], segments[3]);

    let (html_kind, number) = match kind {
        "pulls"   => ("pull",   tail.parse::<i64>().ok()),
        "issues"  => ("issues", tail.parse::<i64>().ok()),
        "commits" => ("commit", None),
        _         => return (None, None),
    };

    let html_url = format!("https://github.com/{owner}/{repo}/{html_kind}/{tail}");
    (Some(html_url), number)
}

fn upsert_notification(
    conn: &Connection,
    thread: &serde_json::Value,
    repo_id_map: &std::collections::HashMap<String, i64>,
    existing_gh_ids: &std::collections::HashSet<String>,
) -> Result<()> {
    let gh_id = match thread["id"].as_str() {
        Some(id) => id,
        None => return Ok(()),
    };

    let owner = thread["repository"]["owner"]["login"].as_str().unwrap_or("");
    let name = thread["repository"]["name"].as_str().unwrap_or("");
    let slug = format!("{owner}/{name}");
    let repo_id = repo_id_map.get(&slug).copied();
    let is_new = !existing_gh_ids.contains(gh_id);

    let subject_url = thread["subject"]["url"].as_str();
    let (html_url, subject_number) = subject_url
        .map(parse_subject_url)
        .unwrap_or((None, None));

    let notif_id: i64 = conn.query_row(
        "INSERT INTO notification
         (gh_id, repo_id, subject_type, subject_title, subject_url, subject_number, html_url,
          reason, unread, updated_at, last_read_at)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)
         ON CONFLICT(gh_id) DO UPDATE SET
             subject_title  = excluded.subject_title,
             subject_url    = excluded.subject_url,
             subject_number = excluded.subject_number,
             html_url       = excluded.html_url,
             reason         = excluded.reason,
             unread         = excluded.unread,
             updated_at     = excluded.updated_at,
             last_read_at   = excluded.last_read_at
         RETURNING id",
        params![
            gh_id,
            repo_id,
            thread["subject"]["type"].as_str().unwrap_or(""),
            thread["subject"]["title"].as_str().unwrap_or(""),
            subject_url,
            subject_number,
            html_url,
            thread["reason"].as_str().unwrap_or(""),
            thread["unread"].as_bool().unwrap_or(true) as i64,
            thread["updated_at"].as_str().unwrap_or(""),
            thread["last_read_at"].as_str(),
        ],
        |r| r.get(0),
    )?;

    let event = if is_new { ChangeEvent::Inserted } else { ChangeEvent::Updated };
    let slug_opt = if owner.is_empty() { None } else { Some(slug.as_str()) };
    db::log_change(conn, "notification", notif_id, event, slug_opt, None)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;

    fn do_upsert(conn: &Connection, thread: &serde_json::Value) -> Result<()> {
        let repo_id_map = load_repo_id_map(conn)?;
        let existing = load_existing_gh_ids(conn)?;
        upsert_notification(conn, thread, &repo_id_map, &existing)
    }

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
        do_upsert(&conn, &thread).unwrap();

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM notification", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn parse_subject_url_pulls() {
        let url = "https://api.github.com/repos/myorg/backend/pulls/42";
        let (html, num) = parse_subject_url(url);
        assert_eq!(html.as_deref(), Some("https://github.com/myorg/backend/pull/42"));
        assert_eq!(num, Some(42));
    }

    #[test]
    fn parse_subject_url_issues() {
        let url = "https://api.github.com/repos/myorg/backend/issues/7";
        let (html, num) = parse_subject_url(url);
        assert_eq!(html.as_deref(), Some("https://github.com/myorg/backend/issues/7"));
        assert_eq!(num, Some(7));
    }

    #[test]
    fn parse_subject_url_commit() {
        let url = "https://api.github.com/repos/myorg/backend/commits/abc123";
        let (html, num) = parse_subject_url(url);
        assert_eq!(html.as_deref(), Some("https://github.com/myorg/backend/commit/abc123"));
        assert_eq!(num, None);
    }

    #[test]
    fn parse_subject_url_unknown() {
        let (html, num) = parse_subject_url("https://example.com/other");
        assert_eq!(html, None);
        assert_eq!(num, None);
    }

    #[test]
    fn upsert_stores_html_url_and_number() {
        let conn = db::open_in_memory().unwrap();
        let thread = make_thread("t99", "o", "n", true);
        do_upsert(&conn, &thread).unwrap();

        let (html_url, subject_number): (Option<String>, Option<i64>) = conn
            .query_row(
                "SELECT html_url, subject_number FROM notification WHERE gh_id='t99'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(html_url.as_deref(), Some("https://github.com/o/n/pull/1"));
        assert_eq!(subject_number, Some(1));
    }

    #[test]
    fn upsert_notification_marks_read() {
        let conn = db::open_in_memory().unwrap();

        let thread = make_thread("thread1", "o", "n", true);
        do_upsert(&conn, &thread).unwrap();

        let mut read = thread.clone();
        read["unread"] = serde_json::json!(false);
        do_upsert(&conn, &read).unwrap();

        let unread: i64 = conn
            .query_row("SELECT unread FROM notification WHERE gh_id='thread1'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(unread, 0);
    }

    #[test]
    fn upsert_notification_idempotent() {
        let conn = db::open_in_memory().unwrap();
        let thread = make_thread("t1", "o", "n", true);
        do_upsert(&conn, &thread).unwrap();
        do_upsert(&conn, &thread).unwrap();

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
        do_upsert(&conn, &thread).unwrap();

        let repo_id: Option<i64> = conn
            .query_row("SELECT repo_id FROM notification WHERE gh_id='t1'", [], |r| r.get(0))
            .unwrap();
        assert!(repo_id.is_some());
    }

    #[test]
    fn v_unread_view() {
        let conn = db::open_in_memory().unwrap();
        do_upsert(&conn, &make_thread("t1", "o", "n", true)).unwrap();
        do_upsert(&conn, &make_thread("t2", "o", "n", false)).unwrap();

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM v_unread_notifications", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }
}
