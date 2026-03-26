use anyhow::Result;
use rusqlite::Connection;

use crate::output::Format;
use super::sql;

pub fn query(
    conn: &Connection,
    mark_read: bool,
    all: bool,
    repo: Option<&str>,
    reason: Option<&str>,
    subject_type: Option<&str>,
    format: Format,
) -> Result<()> {
    if mark_read {
        anyhow::bail!("--mark-read is not yet implemented");
    }

    let mut conditions: Vec<String> = vec![];
    if !all {
        conditions.push("n.unread = 1".into());
    }
    if let Some(slug) = repo {
        let parts: Vec<&str> = slug.splitn(2, '/').collect();
        if parts.len() == 2 {
            conditions.push(format!(
                "r.owner = '{}' AND r.name = '{}'",
                parts[0].replace('\'', "''"),
                parts[1].replace('\'', "''"),
            ));
        }
    }
    if let Some(r) = reason {
        conditions.push(format!("n.reason = '{}'", r.replace('\'', "''")));
    }
    if let Some(t) = subject_type {
        conditions.push(format!("n.subject_type = '{}'", t.replace('\'', "''")));
    }

    let where_clause = if conditions.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", conditions.join(" AND "))
    };

    let query = format!(
        "SELECT n.gh_id, n.subject_type, n.subject_title, n.subject_number,
                n.html_url, n.reason, n.unread, n.updated_at,
                r.owner || '/' || r.name AS repo_slug
         FROM notification n
         LEFT JOIN repo r ON r.id = n.repo_id
         {where_clause}
         ORDER BY n.updated_at DESC"
    );

    sql::query_raw(conn, &query, format)
}
