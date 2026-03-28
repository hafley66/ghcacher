use anyhow::Result;
use sqlx::SqliteConnection;

use crate::db::{log_change, ChangeEvent};
use crate::gh::GitHubClient;

const PR_FIELDS: &str = r#"number title state isDraft body
              headRefName headRefOid baseRefName mergeable
              additions deletions changedFiles
              createdAt updatedAt mergedAt closedAt
              databaseId id
              author { login }
              reviews(last: 20) {
                nodes { databaseId author { login } state body submittedAt }
              }
              reviewRequests(first: 20) {
                nodes {
                  requestedReviewer {
                    ... on User { login }
                    ... on Team { name: slug }
                  }
                }
              }
              labels(first: 10) {
                nodes { name color }
              }
              commits(last: 1) {
                nodes {
                  commit {
                    statusCheckRollup {
                      contexts(first: 50) {
                        nodes {
                          __typename
                          ... on StatusContext { context state targetUrl description }
                          ... on CheckRun { name conclusion detailsUrl }
                        }
                      }
                    }
                  }
                }
              }"#;

#[cfg(test)]
pub async fn upsert_pr_for_test(conn: &mut SqliteConnection, repo_id: i64, pr: &serde_json::Value) -> Result<()> {
    let number = pr["number"].as_i64().unwrap_or(0);
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM pull_request WHERE repo_id=? AND number=?)",
    )
    .bind(repo_id)
    .bind(number)
    .fetch_one(&mut *conn)
    .await?;
    upsert_pr(conn, repo_id, "test/repo", pr, !exists).await
}

pub async fn sync(
    conn: &mut SqliteConnection,
    gh: &dyn GitHubClient,
    repo_id: i64,
    owner: &str,
    name: &str,
) -> Result<()> {
    tracing::debug!(repo = %format!("{owner}/{name}"), "syncing PRs via GraphQL (full)");
    gh.throttle_if_needed(conn, "graphql").await?;

    let inlined = format!(
        r#"{{ repository(owner: "{owner}", name: "{name}") {{
          pullRequests(first: 100, states: OPEN, orderBy: {{field: UPDATED_AT, direction: DESC}}) {{
            nodes {{ {PR_FIELDS} }}
          }}
        }} }}"#
    );

    let endpoint = format!("graphql:prs:full/{owner}/{name}");
    let data  = gh.graphql(conn, &endpoint, &inlined).await?;
    let nodes = &data["repository"]["pullRequests"]["nodes"];
    let nodes = match nodes.as_array() {
        Some(a) => a,
        None => {
            tracing::warn!("no PR nodes returned");
            return Ok(());
        }
    };

    let slug = format!("{owner}/{name}");

    let existing: std::collections::HashSet<i64> = sqlx::query_scalar(
        "SELECT number FROM pull_request WHERE repo_id=?",
    )
    .bind(repo_id)
    .fetch_all(&mut *conn)
    .await?
    .into_iter()
    .collect();

    for pr in nodes {
        let number = pr["number"].as_i64().unwrap_or(0);
        upsert_pr(conn, repo_id, &slug, pr, !existing.contains(&number)).await?;
    }

    tracing::info!(repo = %format!("{owner}/{name}"), count = nodes.len(), "PRs synced");
    Ok(())
}

pub async fn sync_targeted(
    conn: &mut SqliteConnection,
    gh: &dyn GitHubClient,
    repo_id: i64,
    owner: &str,
    name: &str,
    numbers: &[i64],
) -> Result<()> {
    if numbers.is_empty() {
        return Ok(());
    }
    gh.throttle_if_needed(conn, "graphql").await?;

    let aliases: String = numbers
        .iter()
        .map(|n| format!("  pr_{n}: pullRequest(number: {n}) {{\n    {PR_FIELDS}\n  }}"))
        .collect::<Vec<_>>()
        .join("\n");

    let query = format!(
        r#"{{ repository(owner: "{owner}", name: "{name}") {{
{aliases}
        }} }}"#
    );

    let endpoint = format!("graphql:prs:targeted/{owner}/{name}");
    let data = gh.graphql(conn, &endpoint, &query).await?;

    let slug = format!("{owner}/{name}");

    let existing: std::collections::HashSet<i64> = sqlx::query_scalar(
        "SELECT number FROM pull_request WHERE repo_id=?",
    )
    .bind(repo_id)
    .fetch_all(&mut *conn)
    .await?
    .into_iter()
    .collect();

    let repo_data = &data["repository"];
    for n in numbers {
        let key = format!("pr_{n}");
        if let Some(pr) = repo_data.get(&key) {
            if !pr.is_null() {
                upsert_pr(conn, repo_id, &slug, pr, !existing.contains(n)).await?;
            }
        }
    }

    tracing::info!(repo = %slug, count = numbers.len(), "PRs synced (targeted)");
    Ok(())
}

async fn upsert_pr(
    conn: &mut SqliteConnection,
    repo_id: i64,
    repo_slug: &str,
    pr: &serde_json::Value,
    is_new: bool,
) -> Result<()> {
    let number  = pr["number"].as_i64().unwrap_or(0);
    let raw_json = serde_json::to_string(pr)?;

    let state = match pr["state"].as_str().unwrap_or("OPEN") {
        "MERGED" => "merged",
        "CLOSED" => "closed",
        _        => "open",
    };

    let pr_id: i64 = sqlx::query_scalar(
        "INSERT INTO pull_request
         (repo_id, number, gh_node_id, state, title, author, head_ref, head_sha,
          base_ref, mergeable, draft, additions, deletions, changed_files,
          created_at, updated_at, merged_at, closed_at, body, raw_json)
         VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)
         ON CONFLICT(repo_id, number) DO UPDATE SET
             gh_node_id    = excluded.gh_node_id,
             state         = excluded.state,
             title         = excluded.title,
             author        = excluded.author,
             head_ref      = excluded.head_ref,
             head_sha      = excluded.head_sha,
             base_ref      = excluded.base_ref,
             mergeable     = excluded.mergeable,
             draft         = excluded.draft,
             additions     = excluded.additions,
             deletions     = excluded.deletions,
             changed_files = excluded.changed_files,
             updated_at    = excluded.updated_at,
             merged_at     = excluded.merged_at,
             closed_at     = excluded.closed_at,
             body          = excluded.body,
             raw_json      = excluded.raw_json
         RETURNING id",
    )
    .bind(repo_id)
    .bind(number)
    .bind(pr["id"].as_str())
    .bind(state)
    .bind(pr["title"].as_str().unwrap_or(""))
    .bind(pr["author"]["login"].as_str())
    .bind(pr["headRefName"].as_str())
    .bind(pr["headRefOid"].as_str())
    .bind(pr["baseRefName"].as_str())
    .bind(pr["mergeable"].as_str())
    .bind(pr["isDraft"].as_bool().unwrap_or(false) as i64)
    .bind(pr["additions"].as_i64())
    .bind(pr["deletions"].as_i64())
    .bind(pr["changedFiles"].as_i64())
    .bind(pr["createdAt"].as_str().unwrap_or(""))
    .bind(pr["updatedAt"].as_str().unwrap_or(""))
    .bind(pr["mergedAt"].as_str())
    .bind(pr["closedAt"].as_str())
    .bind(pr["body"].as_str())
    .bind(&raw_json)
    .fetch_one(&mut *conn)
    .await?;

    let event = if is_new { ChangeEvent::Inserted } else { ChangeEvent::Updated };
    log_change(conn, "pull_request", pr_id, event, Some(repo_slug), None).await?;

    if let Some(reviews) = pr["reviews"]["nodes"].as_array() {
        for rv in reviews {
            upsert_review(conn, pr_id, rv).await?;
        }
    }

    sqlx::query("DELETE FROM pr_label WHERE pr_id = ?")
        .bind(pr_id)
        .execute(&mut *conn)
        .await?;
    if let Some(labels) = pr["labels"]["nodes"].as_array() {
        for lbl in labels {
            sqlx::query(
                "INSERT OR IGNORE INTO pr_label (pr_id, label, color) VALUES (?, ?, ?)",
            )
            .bind(pr_id)
            .bind(lbl["name"].as_str())
            .bind(lbl["color"].as_str())
            .execute(&mut *conn)
            .await?;
        }
    }

    // Requested reviewers: delete-and-replace (like labels)
    sqlx::query("DELETE FROM pr_requested_reviewer WHERE pr_id = ?")
        .bind(pr_id)
        .execute(&mut *conn)
        .await?;
    if let Some(requests) = pr["reviewRequests"]["nodes"].as_array() {
        for req in requests {
            let reviewer = req["requestedReviewer"]["login"]
                .as_str()
                .or_else(|| req["requestedReviewer"]["name"].as_str());
            if let Some(name) = reviewer {
                sqlx::query(
                    "INSERT OR IGNORE INTO pr_requested_reviewer (pr_id, reviewer) VALUES (?, ?)",
                )
                .bind(pr_id)
                .bind(name)
                .execute(&mut *conn)
                .await?;
            }
        }
    }

    if let Some(commits) = pr["commits"]["nodes"].as_array() {
        if let Some(commit) = commits.first() {
            if let Some(contexts) = commit["commit"]["statusCheckRollup"]["contexts"]["nodes"].as_array() {
                for ctx in contexts {
                    upsert_status_check(conn, pr_id, ctx).await?;
                }
            }
        }
    }

    Ok(())
}

async fn upsert_review(conn: &mut SqliteConnection, pr_id: i64, rv: &serde_json::Value) -> Result<()> {
    let gh_id = rv["databaseId"].as_i64().unwrap_or(0);
    sqlx::query(
        "INSERT INTO pr_review (pr_id, gh_id, author, state, body, submitted_at)
         VALUES (?,?,?,?,?,?)
         ON CONFLICT(pr_id, gh_id) DO UPDATE SET
             state        = excluded.state,
             body         = excluded.body,
             submitted_at = excluded.submitted_at",
    )
    .bind(pr_id)
    .bind(gh_id)
    .bind(rv["author"]["login"].as_str())
    .bind(rv["state"].as_str().unwrap_or(""))
    .bind(rv["body"].as_str())
    .bind(rv["submittedAt"].as_str())
    .execute(conn)
    .await?;
    Ok(())
}

async fn upsert_status_check(conn: &mut SqliteConnection, pr_id: i64, ctx: &serde_json::Value) -> Result<()> {
    let (context, state, target_url, description) = match ctx["__typename"].as_str() {
        Some("StatusContext") => (
            ctx["context"].as_str().unwrap_or(""),
            ctx["state"].as_str().unwrap_or(""),
            ctx["targetUrl"].as_str(),
            ctx["description"].as_str(),
        ),
        Some("CheckRun") => (
            ctx["name"].as_str().unwrap_or(""),
            ctx["conclusion"].as_str().unwrap_or("pending"),
            ctx["detailsUrl"].as_str(),
            None,
        ),
        _ => return Ok(()),
    };

    let now = chrono::Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT INTO pr_status_check (pr_id, context, state, target_url, description, updated_at)
         VALUES (?,?,?,?,?,?)
         ON CONFLICT(pr_id, context) DO UPDATE SET
             state       = excluded.state,
             target_url  = excluded.target_url,
             description = excluded.description,
             updated_at  = excluded.updated_at",
    )
    .bind(pr_id)
    .bind(context)
    .bind(state)
    .bind(target_url)
    .bind(description)
    .bind(&now)
    .execute(conn)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;

    fn make_pr(number: i64, state: &str, title: &str) -> serde_json::Value {
        serde_json::json!({
            "number": number,
            "title": title,
            "state": state,
            "isDraft": false,
            "body": "description",
            "headRefName": "feature-x",
            "headRefOid": "abc123",
            "baseRefName": "main",
            "mergeable": "MERGEABLE",
            "additions": 10,
            "deletions": 2,
            "changedFiles": 3,
            "createdAt": "2026-01-01T00:00:00Z",
            "updatedAt": "2026-01-02T00:00:00Z",
            "mergedAt": null,
            "closedAt": null,
            "databaseId": number,
            "id": format!("PR_node_{number}"),
            "author": { "login": "alice" },
            "reviews": { "nodes": [] },
            "labels": { "nodes": [] },
            "commits": { "nodes": [] }
        })
    }

    #[tokio::test]
    async fn upsert_pr_insert_and_update() {
        let pool = db::open_in_memory().await.unwrap();
        let mut c = pool.acquire().await.unwrap();
        let repo_id = db::upsert_repo(&mut *c, "o", "n", "main").await.unwrap();

        upsert_pr_for_test(&mut *c, repo_id, &make_pr(1, "OPEN", "First")).await.unwrap();

        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pull_request")
            .fetch_one(&mut *c).await.unwrap();
        assert_eq!(count, 1);

        upsert_pr_for_test(&mut *c, repo_id, &make_pr(1, "OPEN", "Updated")).await.unwrap();
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pull_request")
            .fetch_one(&mut *c).await.unwrap();
        assert_eq!(count, 1);

        let title: String = sqlx::query_scalar("SELECT title FROM pull_request WHERE number=1")
            .fetch_one(&mut *c).await.unwrap();
        assert_eq!(title, "Updated");
    }

    #[tokio::test]
    async fn state_mapping() {
        let pool = db::open_in_memory().await.unwrap();
        let mut c = pool.acquire().await.unwrap();
        let repo_id = db::upsert_repo(&mut *c, "o", "n", "main").await.unwrap();
        upsert_pr_for_test(&mut *c, repo_id, &make_pr(1, "MERGED", "x")).await.unwrap();

        let state: String = sqlx::query_scalar("SELECT state FROM pull_request WHERE number=1")
            .fetch_one(&mut *c).await.unwrap();
        assert_eq!(state, "merged");
    }

    #[tokio::test]
    async fn reviews_upserted() {
        let pool = db::open_in_memory().await.unwrap();
        let mut c = pool.acquire().await.unwrap();
        let repo_id = db::upsert_repo(&mut *c, "o", "n", "main").await.unwrap();

        let mut pr = make_pr(1, "OPEN", "x");
        pr["reviews"]["nodes"] = serde_json::json!([
            { "databaseId": 101, "author": { "login": "bob" }, "state": "APPROVED", "body": "", "submittedAt": "2026-01-01T00:00:00Z" }
        ]);
        upsert_pr_for_test(&mut *c, repo_id, &pr).await.unwrap();

        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pr_review")
            .fetch_one(&mut *c).await.unwrap();
        assert_eq!(count, 1);

        let approvals: i64 = sqlx::query_scalar("SELECT approvals FROM v_open_prs WHERE number=1")
            .fetch_one(&mut *c).await.unwrap();
        assert_eq!(approvals, 1);
    }

    #[tokio::test]
    async fn labels_replaced_on_update() {
        let pool = db::open_in_memory().await.unwrap();
        let mut c = pool.acquire().await.unwrap();
        let repo_id = db::upsert_repo(&mut *c, "o", "n", "main").await.unwrap();

        let mut pr = make_pr(1, "OPEN", "x");
        pr["labels"]["nodes"] = serde_json::json!([
            { "name": "bug", "color": "red" },
            { "name": "urgent", "color": "orange" }
        ]);
        upsert_pr_for_test(&mut *c, repo_id, &pr).await.unwrap();

        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pr_label")
            .fetch_one(&mut *c).await.unwrap();
        assert_eq!(count, 2);

        pr["labels"]["nodes"] = serde_json::json!([{ "name": "bug", "color": "red" }]);
        upsert_pr_for_test(&mut *c, repo_id, &pr).await.unwrap();

        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pr_label")
            .fetch_one(&mut *c).await.unwrap();
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn requested_reviewers_synced() {
        let pool = db::open_in_memory().await.unwrap();
        let mut c = pool.acquire().await.unwrap();
        let repo_id = db::upsert_repo(&mut *c, "o", "n", "main").await.unwrap();

        let mut pr = make_pr(1, "OPEN", "x");
        pr["reviewRequests"] = serde_json::json!({"nodes": [
            {"requestedReviewer": {"login": "bob"}},
            {"requestedReviewer": {"login": "carol"}},
        ]});
        upsert_pr_for_test(&mut *c, repo_id, &pr).await.unwrap();

        let reviewers: Vec<String> = sqlx::query_scalar(
            "SELECT reviewer FROM pr_requested_reviewer WHERE pr_id = 1 ORDER BY reviewer",
        )
        .fetch_all(&mut *c)
        .await
        .unwrap();
        assert_eq!(reviewers, vec!["bob", "carol"]);

        // Update: remove carol, add dave
        pr["reviewRequests"] = serde_json::json!({"nodes": [
            {"requestedReviewer": {"login": "bob"}},
            {"requestedReviewer": {"login": "dave"}},
        ]});
        upsert_pr_for_test(&mut *c, repo_id, &pr).await.unwrap();

        let reviewers: Vec<String> = sqlx::query_scalar(
            "SELECT reviewer FROM pr_requested_reviewer WHERE pr_id = 1 ORDER BY reviewer",
        )
        .fetch_all(&mut *c)
        .await
        .unwrap();
        assert_eq!(reviewers, vec!["bob", "dave"]);
    }

    #[tokio::test]
    async fn requested_reviewers_team() {
        let pool = db::open_in_memory().await.unwrap();
        let mut c = pool.acquire().await.unwrap();
        let repo_id = db::upsert_repo(&mut *c, "o", "n", "main").await.unwrap();

        let mut pr = make_pr(1, "OPEN", "x");
        pr["reviewRequests"] = serde_json::json!({"nodes": [
            {"requestedReviewer": {"name": "platform-team"}},
        ]});
        upsert_pr_for_test(&mut *c, repo_id, &pr).await.unwrap();

        let reviewers: Vec<String> = sqlx::query_scalar(
            "SELECT reviewer FROM pr_requested_reviewer WHERE pr_id = 1",
        )
        .fetch_all(&mut *c)
        .await
        .unwrap();
        assert_eq!(reviewers, vec!["platform-team"]);
    }

    #[tokio::test]
    async fn status_checks_upserted() {
        let pool = db::open_in_memory().await.unwrap();
        let mut c = pool.acquire().await.unwrap();
        let repo_id = db::upsert_repo(&mut *c, "o", "n", "main").await.unwrap();

        let mut pr = make_pr(1, "OPEN", "x");
        pr["commits"]["nodes"] = serde_json::json!([{
            "commit": {
                "statusCheckRollup": {
                    "contexts": {
                        "nodes": [
                            { "__typename": "StatusContext", "context": "ci/build", "state": "success", "targetUrl": "http://ci", "description": "ok" },
                            { "__typename": "CheckRun", "name": "test", "conclusion": "success", "detailsUrl": "http://ci/test" }
                        ]
                    }
                }
            }
        }]);
        upsert_pr_for_test(&mut *c, repo_id, &pr).await.unwrap();

        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pr_status_check")
            .fetch_one(&mut *c).await.unwrap();
        assert_eq!(count, 2);
    }
}
