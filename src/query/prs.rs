use anyhow::Result;
use rusqlite::Connection;
use serde::Serialize;

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

pub fn query(
    conn: &Connection,
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

    // `mine` would need the current gh user -- skip for now, document limitation
    if mine {
        tracing::warn!("--mine requires knowing current gh user; not yet implemented");
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

    let mut stmt = conn.prepare(&sql)?;
    let rows: Vec<PrRow> = stmt
        .query_map([], |r| {
            Ok(PrRow {
                repo_slug: r.get(0)?,
                number: r.get(1)?,
                title: r.get(2)?,
                author: r.get(3)?,
                head_ref: r.get(4)?,
                state: r.get(5)?,
                draft: r.get::<_, i64>(6)? != 0,
                mergeable: r.get(7)?,
                additions: r.get(8)?,
                deletions: r.get(9)?,
                created_at: r.get(10)?,
                updated_at: r.get(11)?,
                approvals: r.get(12)?,
                changes_requested: r.get(13)?,
                comment_count: r.get(14)?,
            })
        })?
        .filter_map(|r| r.ok())
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

pub fn query_one(conn: &Connection, number: u32, repo: &str, format: Format) -> Result<()> {
    let parts: Vec<&str> = repo.splitn(2, '/').collect();
    anyhow::ensure!(parts.len() == 2, "repo must be owner/name format");
    let (owner, name) = (parts[0], parts[1]);

    let pr_id: Option<i64> = {
        let mut stmt = conn.prepare(
            "SELECT pr.id FROM pull_request pr
             JOIN repo r ON r.id = pr.repo_id
             WHERE r.owner = ?1 AND r.name = ?2 AND pr.number = ?3",
        )?;
        let mut rows = stmt.query(rusqlite::params![owner, name, number])?;
        rows.next()?.map(|r| r.get_unwrap(0))
    };

    let pr_id = anyhow::Context::context(pr_id, format!("PR #{number} not found in {repo}"))?;

    let detail: PrDetail = {
        let mut stmt = conn.prepare(
            "SELECT r.owner || '/' || r.name, pr.number, pr.title, pr.author, pr.state,
                    pr.draft, pr.mergeable, pr.additions, pr.deletions,
                    pr.created_at, pr.updated_at, pr.body
             FROM pull_request pr JOIN repo r ON r.id = pr.repo_id
             WHERE pr.id = ?1",
        )?;
        stmt.query_row(rusqlite::params![pr_id], |r| {
            Ok(PrDetail {
                repo_slug: r.get(0)?,
                number: r.get(1)?,
                title: r.get(2)?,
                author: r.get(3)?,
                state: r.get(4)?,
                draft: r.get::<_, i64>(5)? != 0,
                mergeable: r.get(6)?,
                additions: r.get(7)?,
                deletions: r.get(8)?,
                created_at: r.get(9)?,
                updated_at: r.get(10)?,
                body: r.get(11)?,
                reviews: vec![],
                comments: vec![],
                labels: vec![],
            })
        })?
    };

    let mut stmt = conn.prepare(
        "SELECT author, state, body, submitted_at FROM pr_review WHERE pr_id = ?1 ORDER BY submitted_at",
    )?;
    let reviews: Vec<ReviewRow> = stmt
        .query_map(rusqlite::params![pr_id], |r| {
            Ok(ReviewRow {
                author: r.get(0)?,
                state: r.get(1)?,
                body: r.get(2)?,
                submitted_at: r.get(3)?,
            })
        })?
        .filter_map(|r| r.ok())
        .collect();

    let mut stmt = conn.prepare(
        "SELECT author, body, path, line, created_at FROM pr_comment WHERE pr_id = ?1 ORDER BY created_at",
    )?;
    let comments: Vec<CommentRow> = stmt
        .query_map(rusqlite::params![pr_id], |r| {
            Ok(CommentRow {
                author: r.get(0)?,
                body: r.get(1)?,
                path: r.get(2)?,
                line: r.get(3)?,
                created_at: r.get(4)?,
            })
        })?
        .filter_map(|r| r.ok())
        .collect();

    let mut stmt = conn.prepare("SELECT label FROM pr_label WHERE pr_id = ?1 ORDER BY label")?;
    let labels: Vec<String> = stmt
        .query_map(rusqlite::params![pr_id], |r| r.get(0))?
        .filter_map(|r| r.ok())
        .collect();

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

    #[test]
    fn query_open_prs() {
        let conn = db::open_in_memory().unwrap();
        let repo_id = db::upsert_repo(&conn, "o", "n", "main").unwrap();
        upsert_pr_for_test(&conn, repo_id, &make_pr(1)).unwrap();
        upsert_pr_for_test(&conn, repo_id, &make_pr(2)).unwrap();

        let sql = "SELECT COUNT(*) FROM pull_request WHERE state='open'";
        let count: i64 = conn.query_row(sql, [], |r| r.get(0)).unwrap();
        assert_eq!(count, 2);
    }

    #[test]
    fn query_needs_review_filter() {
        let conn = db::open_in_memory().unwrap();
        let repo_id = db::upsert_repo(&conn, "o", "n", "main").unwrap();
        upsert_pr_for_test(&conn, repo_id, &make_pr(1)).unwrap();

        let mut pr2 = make_pr(2);
        pr2["reviews"]["nodes"] = serde_json::json!([
            { "databaseId": 1, "author": { "login": "bob" }, "state": "APPROVED", "body": "", "submittedAt": "2026-01-01T00:00:00Z" }
        ]);
        upsert_pr_for_test(&conn, repo_id, &pr2).unwrap();

        // PR 1 has no approvals, PR 2 has one
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pull_request pr WHERE pr.state='open'
                 AND (SELECT COUNT(*) FROM pr_review rv WHERE rv.pr_id=pr.id AND rv.state='APPROVED')=0",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }
}
