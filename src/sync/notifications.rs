use anyhow::Result;
use sqlx::SqliteConnection;

use crate::config::ResolvedConfig;
use crate::db::{self, ChangeEvent};
use crate::gh::{GitHubClient, GhRequest};

const ENDPOINT: &str = "/notifications";

pub async fn sync(
    conn: &mut SqliteConnection,
    gh: &dyn GitHubClient,
    cfg: &ResolvedConfig,
    extra_slugs: &[(String, String)],
) -> Result<()> {
    let poll = db::get_poll_state(conn, ENDPOINT).await?;

    let mut req = GhRequest::get(ENDPOINT);
    if let Some(ref lm) = poll.last_modified {
        req = req.with_last_modified(lm);
    }

    let resp = gh.call(conn, &req).await?;

    if resp.is_not_modified() {
        tracing::debug!("notifications: 304 not modified");
        return Ok(());
    }

    let threads = match resp.body.as_array() {
        Some(a) => a,
        None => return Ok(()),
    };

    let mut tracked_slugs: std::collections::HashSet<String> = cfg
        .repos
        .iter()
        .filter(|r| r.sync_notifications.unwrap_or(false))
        .map(|r| format!("{}/{}", r.owner, r.name))
        .collect();
    for (owner, name) in extra_slugs {
        tracked_slugs.insert(format!("{owner}/{name}"));
    }

    let repo_id_map   = load_repo_id_map(conn).await?;
    let existing_gh_ids = load_existing_gh_ids(conn).await?;

    let mut upserted = 0usize;
    for thread in threads {
        let repo_slug = thread["repository"]["full_name"].as_str().unwrap_or("");
        if !tracked_slugs.is_empty() && !tracked_slugs.contains(repo_slug) {
            continue;
        }

        upsert_notification(conn, thread, &repo_id_map, &existing_gh_ids).await?;
        upserted += 1;
    }

    tracing::info!(upserted, total = threads.len(), "notifications synced");
    Ok(())
}

async fn load_repo_id_map(conn: &mut SqliteConnection) -> Result<std::collections::HashMap<String, i64>> {
    let rows: Vec<(String, i64)> = sqlx::query_as("SELECT owner || '/' || name, id FROM repo")
        .fetch_all(conn)
        .await?;
    Ok(rows.into_iter().collect())
}

async fn load_existing_gh_ids(conn: &mut SqliteConnection) -> Result<std::collections::HashSet<String>> {
    let ids: Vec<String> = sqlx::query_scalar("SELECT gh_id FROM notification")
        .fetch_all(conn)
        .await?;
    Ok(ids.into_iter().collect())
}

fn parse_subject_url(api_url: &str) -> (Option<String>, Option<i64>) {
    let path = if let Some(rest) = api_url.strip_prefix("https://api.github.com/repos/") {
        rest
    } else {
        return (None, None);
    };

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

async fn upsert_notification(
    conn: &mut SqliteConnection,
    thread: &serde_json::Value,
    repo_id_map: &std::collections::HashMap<String, i64>,
    existing_gh_ids: &std::collections::HashSet<String>,
) -> Result<()> {
    let gh_id = match thread["id"].as_str() {
        Some(id) => id,
        None => return Ok(()),
    };

    let owner = thread["repository"]["owner"]["login"].as_str().unwrap_or("");
    let name  = thread["repository"]["name"].as_str().unwrap_or("");
    let slug  = format!("{owner}/{name}");
    let repo_id = repo_id_map.get(&slug).copied();
    let is_new  = !existing_gh_ids.contains(gh_id);

    let subject_url = thread["subject"]["url"].as_str();
    let (html_url, subject_number) = subject_url
        .map(parse_subject_url)
        .unwrap_or((None, None));

    let notif_id: i64 = sqlx::query_scalar(
        "INSERT INTO notification
         (gh_id, repo_id, subject_type, subject_title, subject_url, subject_number, html_url,
          reason, unread, updated_at, last_read_at)
         VALUES (?,?,?,?,?,?,?,?,?,?,?)
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
    )
    .bind(gh_id)
    .bind(repo_id)
    .bind(thread["subject"]["type"].as_str().unwrap_or(""))
    .bind(thread["subject"]["title"].as_str().unwrap_or(""))
    .bind(subject_url)
    .bind(subject_number)
    .bind(html_url)
    .bind(thread["reason"].as_str().unwrap_or(""))
    .bind(thread["unread"].as_bool().unwrap_or(true) as i64)
    .bind(thread["updated_at"].as_str().unwrap_or(""))
    .bind(thread["last_read_at"].as_str())
    .fetch_one(&mut *conn)
    .await?;

    let event    = if is_new { ChangeEvent::Inserted } else { ChangeEvent::Updated };
    let slug_opt = if owner.is_empty() { None } else { Some(slug.as_str()) };
    db::log_change(&mut *conn, "notification", notif_id, event, slug_opt, None).await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;

    async fn do_upsert(conn: &mut SqliteConnection, thread: &serde_json::Value) {
        let repo_id_map   = load_repo_id_map(conn).await.unwrap();
        let existing      = load_existing_gh_ids(conn).await.unwrap();
        upsert_notification(conn, thread, &repo_id_map, &existing).await.unwrap();
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

    #[tokio::test]
    async fn upsert_notification_insert() {
        let pool = db::open_in_memory().await.unwrap();
        let mut c = pool.acquire().await.unwrap();
        do_upsert(&mut *c, &make_thread("thread1", "o", "n", true)).await;
        drop(c);

        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM notification")
            .fetch_one(&pool)
            .await
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

    #[tokio::test]
    async fn upsert_stores_html_url_and_number() {
        let pool = db::open_in_memory().await.unwrap();
        let mut c = pool.acquire().await.unwrap();
        do_upsert(&mut *c, &make_thread("t99", "o", "n", true)).await;
        drop(c);

        let row: (Option<String>, Option<i64>) = sqlx::query_as(
            "SELECT html_url, subject_number FROM notification WHERE gh_id='t99'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(row.0.as_deref(), Some("https://github.com/o/n/pull/1"));
        assert_eq!(row.1, Some(1));
    }

    #[tokio::test]
    async fn upsert_notification_marks_read() {
        let pool = db::open_in_memory().await.unwrap();
        let mut c = pool.acquire().await.unwrap();
        do_upsert(&mut *c, &make_thread("thread1", "o", "n", true)).await;

        let mut read = make_thread("thread1", "o", "n", false);
        read["unread"] = serde_json::json!(false);
        do_upsert(&mut *c, &read).await;
        drop(c);

        let unread: i64 = sqlx::query_scalar("SELECT unread FROM notification WHERE gh_id='thread1'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(unread, 0);
    }

    #[tokio::test]
    async fn upsert_notification_idempotent() {
        let pool = db::open_in_memory().await.unwrap();
        let mut c = pool.acquire().await.unwrap();
        let thread = make_thread("t1", "o", "n", true);
        do_upsert(&mut *c, &thread).await;
        do_upsert(&mut *c, &thread).await;
        drop(c);

        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM notification")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn notification_links_repo_id() {
        let pool = db::open_in_memory().await.unwrap();
        let mut c = pool.acquire().await.unwrap();
        db::upsert_repo(&mut *c, "o", "n", "main").await.unwrap();

        do_upsert(&mut *c, &make_thread("t1", "o", "n", true)).await;
        drop(c);

        let repo_id: Option<i64> = sqlx::query_scalar("SELECT repo_id FROM notification WHERE gh_id='t1'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(repo_id.is_some());
    }

    #[tokio::test]
    async fn v_unread_view() {
        let pool = db::open_in_memory().await.unwrap();
        let mut c = pool.acquire().await.unwrap();
        do_upsert(&mut *c, &make_thread("t1", "o", "n", true)).await;
        do_upsert(&mut *c, &make_thread("t2", "o", "n", false)).await;
        drop(c);

        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM v_unread_notifications")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 1);
    }
}
