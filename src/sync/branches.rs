use anyhow::Result;
use rusqlite::{Connection, params};

use crate::db;
use crate::gh::{GhClient, GhRequest};

pub fn sync(
    conn: &Connection,
    gh: &GhClient,
    repo_id: i64,
    owner: &str,
    name: &str,
    patterns: &[String],
) -> Result<()> {
    let endpoint = format!("/repos/{owner}/{name}/branches");
    let poll = db::get_poll_state(conn, &endpoint)?;

    let mut req = GhRequest::get(&endpoint).paginated();
    if let Some(ref etag) = poll.etag {
        req = req.with_etag(etag);
    }

    let resp = gh.call(conn, &req)?;

    if resp.is_not_modified() {
        tracing::debug!(repo = %format!("{owner}/{name}"), "branches: 304 not modified");
        return Ok(());
    }

    let branches = match resp.body.as_array() {
        Some(a) => a,
        None => return Ok(()),
    };

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

        conn.execute(
            "INSERT INTO branch (repo_id, name, sha, updated_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(repo_id, name) DO UPDATE SET
                 sha        = excluded.sha,
                 updated_at = excluded.updated_at",
            params![repo_id, branch_name, sha, now],
        )?;
        synced += 1;
    }

    tracing::info!(repo = %format!("{owner}/{name}"), synced, "branches synced");
    Ok(())
}

fn matches_glob(pattern: &str, name: &str) -> bool {
    // Simple glob: only support trailing `*`
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
        serde_json::json!({
            "name": name,
            "commit": { "sha": sha }
        })
    }

    fn insert_branches(conn: &Connection, repo_id: i64, branches: &[serde_json::Value], patterns: &[&str]) {
        for branch in branches {
            let branch_name = branch["name"].as_str().unwrap();
            if !patterns.iter().any(|pat| matches_glob(pat, branch_name)) {
                continue;
            }
            let sha = branch["commit"]["sha"].as_str();
            let now = chrono::Utc::now().to_rfc3339();
            conn.execute(
                "INSERT INTO branch (repo_id, name, sha, updated_at)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(repo_id, name) DO UPDATE SET sha = excluded.sha, updated_at = excluded.updated_at",
                params![repo_id, branch_name, sha, now],
            ).unwrap();
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

    #[test]
    fn branches_filtered_by_pattern() {
        let conn = db::open_in_memory().unwrap();
        let repo_id = db::upsert_repo(&conn, "o", "n", "main").unwrap();

        let branches = vec![
            make_branch("main", "aaa"),
            make_branch("feature-x", "bbb"),
            make_branch("release/1.0", "ccc"),
        ];
        insert_branches(&conn, repo_id, &branches, &["main", "release/*"]);

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM branch", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 2);
    }

    #[test]
    fn branch_sha_updates_on_upsert() {
        let conn = db::open_in_memory().unwrap();
        let repo_id = db::upsert_repo(&conn, "o", "n", "main").unwrap();

        insert_branches(&conn, repo_id, &[make_branch("main", "old_sha")], &["main"]);
        insert_branches(&conn, repo_id, &[make_branch("main", "new_sha")], &["main"]);

        let sha: String = conn
            .query_row("SELECT sha FROM branch WHERE name='main'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(sha, "new_sha");

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM branch", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }
}
