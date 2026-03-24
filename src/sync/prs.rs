use anyhow::Result;
use rusqlite::{Connection, params};

use crate::db::{log_change, ChangeEvent};
use crate::gh::GhClient;


/// Exposed for tests in query modules.
#[cfg(test)]
pub fn upsert_pr_for_test(conn: &Connection, repo_id: i64, pr: &serde_json::Value) -> Result<()> {
    upsert_pr(conn, repo_id, "test/repo", pr)
}

pub fn sync(conn: &Connection, gh: &GhClient, repo_id: i64, owner: &str, name: &str) -> Result<()> {
    tracing::debug!(repo = %format!("{owner}/{name}"), "syncing PRs via GraphQL");

    // Build the query with variables inlined (gh api graphql -f doesn't support $vars easily)
    let inlined = format!(
        r#"{{ repository(owner: "{owner}", name: "{name}") {{
          pullRequests(first: 100, states: OPEN, orderBy: {{field: UPDATED_AT, direction: DESC}}) {{
            nodes {{
              number title state isDraft body
              headRefName headRefOid baseRefName mergeable
              additions deletions changedFiles
              createdAt updatedAt mergedAt closedAt
              databaseId id
              author {{ login }}
              reviews(last: 20) {{
                nodes {{ databaseId author {{ login }} state body submittedAt }}
              }}
              labels(first: 10) {{
                nodes {{ name color }}
              }}
              commits(last: 1) {{
                nodes {{
                  commit {{
                    statusCheckRollup {{
                      contexts(first: 50) {{
                        nodes {{
                          __typename
                          ... on StatusContext {{ context state targetUrl description }}
                          ... on CheckRun {{ name conclusion detailsUrl }}
                        }}
                      }}
                    }}
                  }}
                }}
              }}
            }}
          }}
        }} }}"#
    );

    let data = gh.graphql(conn, &inlined)?;
    let nodes = &data["repository"]["pullRequests"]["nodes"];
    let nodes = match nodes.as_array() {
        Some(a) => a,
        None => {
            tracing::warn!("no PR nodes returned");
            return Ok(());
        }
    };

    let slug = format!("{owner}/{name}");
    for pr in nodes {
        upsert_pr(conn, repo_id, &slug, pr)?;
    }

    tracing::info!(repo = %format!("{owner}/{name}"), count = nodes.len(), "PRs synced");
    Ok(())
}

fn upsert_pr(conn: &Connection, repo_id: i64, repo_slug: &str, pr: &serde_json::Value) -> Result<()> {
    let number = pr["number"].as_i64().unwrap_or(0);
    let raw_json = serde_json::to_string(pr)?;

    let state = match pr["state"].as_str().unwrap_or("OPEN") {
        "MERGED" => "merged",
        "CLOSED" => "closed",
        _ => "open",
    };

    conn.execute(
        "INSERT INTO pull_request
         (repo_id, number, gh_node_id, state, title, author, head_ref, head_sha,
          base_ref, mergeable, draft, additions, deletions, changed_files,
          created_at, updated_at, merged_at, closed_at, body, raw_json)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20)
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
             raw_json      = excluded.raw_json",
        params![
            repo_id,
            number,
            pr["id"].as_str(),
            state,
            pr["title"].as_str().unwrap_or(""),
            pr["author"]["login"].as_str(),
            pr["headRefName"].as_str(),
            pr["headRefOid"].as_str(),
            pr["baseRefName"].as_str(),
            pr["mergeable"].as_str(),
            pr["isDraft"].as_bool().unwrap_or(false) as i64,
            pr["additions"].as_i64(),
            pr["deletions"].as_i64(),
            pr["changedFiles"].as_i64(),
            pr["createdAt"].as_str().unwrap_or(""),
            pr["updatedAt"].as_str().unwrap_or(""),
            pr["mergedAt"].as_str(),
            pr["closedAt"].as_str(),
            pr["body"].as_str(),
            raw_json,
        ],
    )?;

    let pr_id: i64 = conn.query_row(
        "SELECT id FROM pull_request WHERE repo_id = ?1 AND number = ?2",
        params![repo_id, number],
        |r| r.get(0),
    )?;

    // Detect insert vs update: rowid changes on true insert, stays on conflict-update
    let event = if conn.last_insert_rowid() == pr_id {
        ChangeEvent::Inserted
    } else {
        ChangeEvent::Updated
    };
    log_change(conn, "pull_request", pr_id, event, Some(repo_slug), None)?;

    // Reviews
    if let Some(reviews) = pr["reviews"]["nodes"].as_array() {
        for rv in reviews {
            upsert_review(conn, pr_id, rv)?;
        }
    }

    // Labels -- replace all
    conn.execute("DELETE FROM pr_label WHERE pr_id = ?1", params![pr_id])?;
    if let Some(labels) = pr["labels"]["nodes"].as_array() {
        for lbl in labels {
            conn.execute(
                "INSERT OR IGNORE INTO pr_label (pr_id, label, color) VALUES (?1, ?2, ?3)",
                params![pr_id, lbl["name"].as_str(), lbl["color"].as_str()],
            )?;
        }
    }

    // Status checks
    if let Some(commits) = pr["commits"]["nodes"].as_array() {
        if let Some(commit) = commits.first() {
            if let Some(contexts) = commit["commit"]["statusCheckRollup"]["contexts"]["nodes"].as_array() {
                for ctx in contexts {
                    upsert_status_check(conn, pr_id, ctx)?;
                }
            }
        }
    }

    Ok(())
}

fn upsert_review(conn: &Connection, pr_id: i64, rv: &serde_json::Value) -> Result<()> {
    let gh_id = rv["databaseId"].as_i64().unwrap_or(0);
    conn.execute(
        "INSERT INTO pr_review (pr_id, gh_id, author, state, body, submitted_at)
         VALUES (?1,?2,?3,?4,?5,?6)
         ON CONFLICT(pr_id, gh_id) DO UPDATE SET
             state        = excluded.state,
             body         = excluded.body,
             submitted_at = excluded.submitted_at",
        params![
            pr_id,
            gh_id,
            rv["author"]["login"].as_str(),
            rv["state"].as_str().unwrap_or(""),
            rv["body"].as_str(),
            rv["submittedAt"].as_str(),
        ],
    )?;
    Ok(())
}

fn upsert_status_check(conn: &Connection, pr_id: i64, ctx: &serde_json::Value) -> Result<()> {
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
    conn.execute(
        "INSERT INTO pr_status_check (pr_id, context, state, target_url, description, updated_at)
         VALUES (?1,?2,?3,?4,?5,?6)
         ON CONFLICT(pr_id, context) DO UPDATE SET
             state       = excluded.state,
             target_url  = excluded.target_url,
             description = excluded.description,
             updated_at  = excluded.updated_at",
        params![pr_id, context, state, target_url, description, now],
    )?;
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

    #[test]
    fn upsert_pr_insert_and_update() {
        let conn = db::open_in_memory().unwrap();
        let repo_id = db::upsert_repo(&conn, "o", "n", "main").unwrap();

        upsert_pr(&conn, repo_id, "o/n", &make_pr(1, "OPEN", "First")).unwrap();

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM pull_request", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);

        // Update title via upsert
        upsert_pr(&conn, repo_id, "o/n", &make_pr(1, "OPEN", "Updated")).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM pull_request", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);

        let title: String = conn
            .query_row("SELECT title FROM pull_request WHERE number=1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(title, "Updated");
    }

    #[test]
    fn state_mapping() {
        let conn = db::open_in_memory().unwrap();
        let repo_id = db::upsert_repo(&conn, "o", "n", "main").unwrap();

        upsert_pr(&conn, repo_id, "o/n", &make_pr(1, "MERGED", "x")).unwrap();
        let state: String = conn
            .query_row("SELECT state FROM pull_request WHERE number=1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(state, "merged");
    }

    #[test]
    fn reviews_upserted() {
        let conn = db::open_in_memory().unwrap();
        let repo_id = db::upsert_repo(&conn, "o", "n", "main").unwrap();

        let mut pr = make_pr(1, "OPEN", "x");
        pr["reviews"]["nodes"] = serde_json::json!([
            { "databaseId": 101, "author": { "login": "bob" }, "state": "APPROVED", "body": "", "submittedAt": "2026-01-01T00:00:00Z" }
        ]);
        upsert_pr(&conn, repo_id, "o/n", &pr).unwrap();

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM pr_review", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);

        let approvals: i64 = conn
            .query_row("SELECT approvals FROM v_open_prs WHERE number=1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(approvals, 1);
    }

    #[test]
    fn labels_replaced_on_update() {
        let conn = db::open_in_memory().unwrap();
        let repo_id = db::upsert_repo(&conn, "o", "n", "main").unwrap();

        let mut pr = make_pr(1, "OPEN", "x");
        pr["labels"]["nodes"] = serde_json::json!([
            { "name": "bug", "color": "red" },
            { "name": "urgent", "color": "orange" }
        ]);
        upsert_pr(&conn, repo_id, "o/n", &pr).unwrap();

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM pr_label", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 2);

        // Re-sync with one label removed
        pr["labels"]["nodes"] = serde_json::json!([{ "name": "bug", "color": "red" }]);
        upsert_pr(&conn, repo_id, "o/n", &pr).unwrap();

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM pr_label", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn status_checks_upserted() {
        let conn = db::open_in_memory().unwrap();
        let repo_id = db::upsert_repo(&conn, "o", "n", "main").unwrap();

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
        upsert_pr(&conn, repo_id, "o/n", &pr).unwrap();

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM pr_status_check", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 2);
    }
}
