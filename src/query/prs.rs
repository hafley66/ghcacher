use anyhow::Result;
use serde::Serialize;
use sqlx::{Row, SqlitePool};

use crate::output::{self, Format};

#[derive(Serialize)]
pub struct PrRow {
    pub repo_slug: String,
    pub number: i64,
    pub title: String,
    pub author: Option<String>,
    pub head_ref: Option<String>,
    pub state: String,
    pub draft: bool,
    pub mergeable: Option<String>,
    pub additions: Option<i64>,
    pub deletions: Option<i64>,
    pub approvals: i64,
    pub changes_requested: i64,
    pub comment_count: i64,
    pub created_at: String,
    pub updated_at: String,
}

pub async fn query(
    pool: &SqlitePool,
    repo: Option<&str>,
    state: &str,
    needs_review: bool,
    mine: bool,
    format: Format,
) -> Result<()> {
    let mut conditions = vec![format!("pr.state = '{}'", state.replace('\'', "''"))];

    if let Some(slug) = repo {
        let parts: Vec<&str> = slug.splitn(2, '/').collect();
        if parts.len() == 2 {
            conditions.push(format!(
                "r.owner = '{}' AND r.name = '{}'",
                parts[0].replace('\'', "''"),
                parts[1].replace('\'', "''")
            ));
        }
    }

    if needs_review {
        conditions.push(
            "(SELECT COUNT(*) FROM pr_review rv WHERE rv.pr_id = pr.id AND rv.state = 'APPROVED') = 0"
                .into(),
        );
    }

    if mine {
        match tokio::process::Command::new("gh")
            .args(["api", "user", "--jq", ".login"])
            .output()
            .await
        {
            Ok(out) if out.status.success() => {
                let login = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if !login.is_empty() {
                    conditions.push(format!("pr.author = '{}'", login.replace('\'', "''")));
                }
            }
            _ => tracing::warn!("--mine: could not resolve gh user; ignoring filter"),
        }
    }

    let where_clause = conditions.join(" AND ");

    let sql = format!(
        "SELECT
             r.owner || '/' || r.name AS repo_slug,
             pr.number, pr.title, pr.author, pr.head_ref, pr.state,
             pr.draft, pr.mergeable, pr.additions, pr.deletions,
             pr.created_at, pr.updated_at,
             (SELECT COUNT(*) FROM pr_review rv WHERE rv.pr_id = pr.id AND rv.state = 'APPROVED') AS approvals,
             (SELECT COUNT(*) FROM pr_review rv WHERE rv.pr_id = pr.id AND rv.state = 'CHANGES_REQUESTED') AS changes_requested,
             (SELECT COUNT(*) FROM pr_comment c WHERE c.pr_id = pr.id) AS comment_count
         FROM pull_request pr
         JOIN repo r ON r.id = pr.repo_id
         WHERE {where_clause}
         ORDER BY pr.updated_at DESC"
    );

    let rows: Vec<PrRow> = sqlx::query(&sql)
        .fetch_all(pool)
        .await?
        .iter()
        .map(|r| PrRow {
            repo_slug: r.get(0),
            number: r.get(1),
            title: r.get(2),
            author: r.get(3),
            head_ref: r.get(4),
            state: r.get(5),
            draft: r.get::<i64, _>(6) != 0,
            mergeable: r.get(7),
            additions: r.get(8),
            deletions: r.get(9),
            created_at: r.get(10),
            updated_at: r.get(11),
            approvals: r.get(12),
            changes_requested: r.get(13),
            comment_count: r.get(14),
        })
        .collect();

    output::print_rows(
        &rows,
        format,
        &["repo_slug", "number", "title", "author", "state", "approvals", "updated_at"],
    )
}

#[derive(Serialize)]
pub struct PrDetail {
    pub repo_slug: String,
    pub number: i64,
    pub title: String,
    pub author: Option<String>,
    pub state: String,
    pub draft: bool,
    pub mergeable: Option<String>,
    pub additions: Option<i64>,
    pub deletions: Option<i64>,
    pub created_at: String,
    pub updated_at: String,
    pub body: Option<String>,
    pub reviews: Vec<ReviewRow>,
    pub comments: Vec<CommentRow>,
    pub labels: Vec<String>,
}

#[derive(Serialize)]
pub struct ReviewRow {
    pub author: Option<String>,
    pub state: String,
    pub body: Option<String>,
    pub submitted_at: Option<String>,
}

#[derive(Serialize)]
pub struct CommentRow {
    pub author: Option<String>,
    pub body: String,
    pub path: Option<String>,
    pub line: Option<i64>,
    pub created_at: String,
}

pub async fn query_one(pool: &SqlitePool, number: u32, repo: &str, format: Format) -> Result<()> {
    let parts: Vec<&str> = repo.splitn(2, '/').collect();
    anyhow::ensure!(parts.len() == 2, "repo must be owner/name format");
    let (owner, name) = (parts[0], parts[1]);

    let pr_id: Option<i64> = sqlx::query_scalar(
        "SELECT pr.id FROM pull_request pr
         JOIN repo r ON r.id = pr.repo_id
         WHERE r.owner = ? AND r.name = ? AND pr.number = ?",
    )
    .bind(owner)
    .bind(name)
    .bind(number as i64)
    .fetch_optional(pool)
    .await?;

    let pr_id = anyhow::Context::context(pr_id, format!("PR #{number} not found in {repo}"))?;

    let row = sqlx::query(
        "SELECT r.owner || '/' || r.name, pr.number, pr.title, pr.author, pr.state,
                pr.draft, pr.mergeable, pr.additions, pr.deletions,
                pr.created_at, pr.updated_at, pr.body
         FROM pull_request pr JOIN repo r ON r.id = pr.repo_id
         WHERE pr.id = ?",
    )
    .bind(pr_id)
    .fetch_one(pool)
    .await?;

    let detail = PrDetail {
        repo_slug: row.get(0),
        number: row.get(1),
        title: row.get(2),
        author: row.get(3),
        state: row.get(4),
        draft: row.get::<i64, _>(5) != 0,
        mergeable: row.get(6),
        additions: row.get(7),
        deletions: row.get(8),
        created_at: row.get(9),
        updated_at: row.get(10),
        body: row.get(11),
        reviews: vec![],
        comments: vec![],
        labels: vec![],
    };

    let reviews: Vec<ReviewRow> = sqlx::query(
        "SELECT author, state, body, submitted_at FROM pr_review WHERE pr_id = ? ORDER BY submitted_at",
    )
    .bind(pr_id)
    .fetch_all(pool)
    .await?
    .iter()
    .map(|r| ReviewRow {
        author: r.get(0),
        state: r.get(1),
        body: r.get(2),
        submitted_at: r.get(3),
    })
    .collect();

    let comments: Vec<CommentRow> = sqlx::query(
        "SELECT author, body, path, line, created_at FROM pr_comment WHERE pr_id = ? ORDER BY created_at",
    )
    .bind(pr_id)
    .fetch_all(pool)
    .await?
    .iter()
    .map(|r| CommentRow {
        author: r.get(0),
        body: r.get(1),
        path: r.get(2),
        line: r.get(3),
        created_at: r.get(4),
    })
    .collect();

    let labels: Vec<String> = sqlx::query_scalar("SELECT label FROM pr_label WHERE pr_id = ? ORDER BY label")
        .bind(pr_id)
        .fetch_all(pool)
        .await?;

    let full = PrDetail { reviews, comments, labels, ..detail };
    output::print_rows(&[full], format, &["repo_slug", "number", "title", "state", "author"])
}

#[cfg(test)]
mod tests {
    use crate::db;
    use crate::sync::prs::upsert_pr_for_test;

    fn make_pr(number: i64) -> serde_json::Value {
        serde_json::json!({
            "number": number,
            "title": format!("PR #{number}"),
            "state": "OPEN",
            "isDraft": false,
            "body": null,
            "headRefName": "branch",
            "headRefOid": "sha",
            "baseRefName": "main",
            "mergeable": "MERGEABLE",
            "additions": 5,
            "deletions": 1,
            "changedFiles": 1,
            "createdAt": "2026-01-01T00:00:00Z",
            "updatedAt": "2026-01-02T00:00:00Z",
            "mergedAt": null,
            "closedAt": null,
            "databaseId": number,
            "id": format!("node_{number}"),
            "author": { "login": "alice" },
            "reviews": { "nodes": [] },
            "labels": { "nodes": [] },
            "commits": { "nodes": [] }
        })
    }

    #[tokio::test]
    async fn query_open_prs() {
        let pool = db::open_in_memory().await.unwrap();
        let mut conn = pool.acquire().await.unwrap();
        let repo_id = db::upsert_repo(&mut *conn, "o", "n", "main").await.unwrap();
        upsert_pr_for_test(&mut *conn, repo_id, &make_pr(1)).await.unwrap();
        upsert_pr_for_test(&mut *conn, repo_id, &make_pr(2)).await.unwrap();
        drop(conn);

        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pull_request WHERE state='open'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 2);
    }

    #[tokio::test]
    async fn query_needs_review_filter() {
        let pool = db::open_in_memory().await.unwrap();
        let mut conn = pool.acquire().await.unwrap();
        let repo_id = db::upsert_repo(&mut *conn, "o", "n", "main").await.unwrap();
        upsert_pr_for_test(&mut *conn, repo_id, &make_pr(1)).await.unwrap();

        let mut pr2 = make_pr(2);
        pr2["reviews"]["nodes"] = serde_json::json!([
            { "databaseId": 1, "author": { "login": "bob" }, "state": "APPROVED", "body": "", "submittedAt": "2026-01-01T00:00:00Z" }
        ]);
        upsert_pr_for_test(&mut *conn, repo_id, &pr2).await.unwrap();
        drop(conn);

        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM pull_request pr WHERE pr.state='open'
             AND (SELECT COUNT(*) FROM pr_review rv WHERE rv.pr_id=pr.id AND rv.state='APPROVED')=0",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(count, 1);
    }
}
