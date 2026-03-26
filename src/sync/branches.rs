use anyhow::Result;
use sqlx::SqliteConnection;

use crate::db::{self, ChangeEvent};
use crate::gh::{GitHubClient, GhRequest};

pub async fn sync(
    conn: &mut SqliteConnection,
    gh: &dyn GitHubClient,
    repo_id: i64,
    owner: &str,
    name: &str,
    patterns: &[String],
) -> Result<()> {
    let endpoint = format!("/repos/{owner}/{name}/branches");
    let poll = db::get_poll_state(conn, &endpoint).await?;

    let mut req = GhRequest::get(&endpoint).paginated();
    if let Some(ref etag) = poll.etag {
        req = req.with_etag(etag);
    }

    let resp = gh.call(conn, &req).await?;

    if resp.is_not_modified() {
        tracing::debug!(repo = %format!("{owner}/{name}"), "branches: 304 not modified");
        return Ok(());
    }

    let branches = match resp.body.as_array() {
        Some(a) => a,
        None => return Ok(()),
    };

    let slug = format!("{owner}/{name}");

    let existing: std::collections::HashSet<String> = sqlx::query_scalar(
        "SELECT name FROM branch WHERE repo_id = ?",
    )
    .bind(repo_id)
    .fetch_all(&mut *conn)
    .await?
    .into_iter()
    .collect();

    let mut synced = 0usize;
    for branch in branches {
        let branch_name = match branch["name"].as_str() {
            Some(n) => n,
            None => continue,
        };

        if !patterns.iter().any(|pat| matches_glob(pat, branch_name)) {
            continue;
        }

        let sha = branch["commit"]["sha"].as_str();
        let now = chrono::Utc::now().to_rfc3339();
        let is_new = !existing.contains(branch_name);

        let branch_id: i64 = sqlx::query_scalar(
            "INSERT INTO branch (repo_id, name, sha, updated_at)
             VALUES (?, ?, ?, ?)
             ON CONFLICT(repo_id, name) DO UPDATE SET
                 sha        = excluded.sha,
                 updated_at = excluded.updated_at
             RETURNING id",
        )
        .bind(repo_id)
        .bind(branch_name)
        .bind(sha)
        .bind(&now)
        .fetch_one(&mut *conn)
        .await?;

        let event = if is_new { ChangeEvent::Inserted } else { ChangeEvent::Updated };
        db::log_change(conn, "branch", branch_id, event, Some(&slug), None).await?;
        synced += 1;
    }

    tracing::info!(repo = %format!("{owner}/{name}"), synced, "branches synced");
    Ok(())
}

fn matches_glob(pattern: &str, name: &str) -> bool {
    if let Some(prefix) = pattern.strip_suffix('*') {
        return name.starts_with(prefix);
    }
    pattern == name
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;

    fn make_branch(name: &str, sha: &str) -> serde_json::Value {
        serde_json::json!({ "name": name, "commit": { "sha": sha } })
    }

    async fn insert_branches(conn: &mut SqliteConnection, repo_id: i64, branches: &[serde_json::Value], patterns: &[&str]) {
        for branch in branches {
            let branch_name = branch["name"].as_str().unwrap();
            if !patterns.iter().any(|pat| matches_glob(pat, branch_name)) {
                continue;
            }
            let sha = branch["commit"]["sha"].as_str();
            let now = chrono::Utc::now().to_rfc3339();
            sqlx::query(
                "INSERT INTO branch (repo_id, name, sha, updated_at)
                 VALUES (?, ?, ?, ?)
                 ON CONFLICT(repo_id, name) DO UPDATE SET sha = excluded.sha, updated_at = excluded.updated_at",
            )
            .bind(repo_id)
            .bind(branch_name)
            .bind(sha)
            .bind(&now)
            .execute(&mut *conn)
            .await
            .unwrap();
        }
    }

    #[test]
    fn glob_exact_match() {
        assert!(matches_glob("main", "main"));
        assert!(!matches_glob("main", "master"));
    }

    #[test]
    fn glob_wildcard() {
        assert!(matches_glob("release/*", "release/1.0"));
        assert!(matches_glob("release/*", "release/2.0-rc"));
        assert!(!matches_glob("release/*", "hotfix/thing"));
    }

    #[tokio::test]
    async fn branches_filtered_by_pattern() {
        let pool = db::open_in_memory().await.unwrap();
        let mut c = pool.acquire().await.unwrap();
        let repo_id = db::upsert_repo(&mut *c, "o", "n", "main").await.unwrap();

        let branches = vec![
            make_branch("main", "aaa"),
            make_branch("feature-x", "bbb"),
            make_branch("release/1.0", "ccc"),
        ];
        insert_branches(&mut *c, repo_id, &branches, &["main", "release/*"]).await;
        drop(c);

        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM branch")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 2);
    }

    #[tokio::test]
    async fn branch_sha_updates_on_upsert() {
        let pool = db::open_in_memory().await.unwrap();
        let mut c = pool.acquire().await.unwrap();
        let repo_id = db::upsert_repo(&mut *c, "o", "n", "main").await.unwrap();

        insert_branches(&mut *c, repo_id, &[make_branch("main", "old_sha")], &["main"]).await;
        insert_branches(&mut *c, repo_id, &[make_branch("main", "new_sha")], &["main"]).await;
        drop(c);

        let sha: String = sqlx::query_scalar("SELECT sha FROM branch WHERE name='main'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(sha, "new_sha");

        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM branch")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 1);
    }
}
