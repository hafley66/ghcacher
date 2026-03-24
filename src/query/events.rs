use anyhow::Result;
use rusqlite::Connection;

use crate::output::Format;
use super::sql;

pub fn query(conn: &Connection, repo: Option<&str>, event_type: Option<&str>, format: Format) -> Result<()> {
    let mut conditions = vec!["1=1".to_owned()];

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

    if let Some(t) = event_type {
        conditions.push(format!("e.type = '{}'", t.replace('\'', "''")));
    }

    let where_clause = conditions.join(" AND ");
    let query = format!(
        "SELECT r.owner || '/' || r.name AS repo_slug, e.type, e.actor, e.created_at,
                json_extract(e.payload_json, '$.action') AS action
         FROM repo_event e
         JOIN repo r ON r.id = e.repo_id
         WHERE {where_clause}
         ORDER BY e.created_at DESC
         LIMIT 100"
    );

    sql::query_raw(conn, &query, format)
}
