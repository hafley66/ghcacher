use anyhow::Result;
use rusqlite::Connection;

use crate::output::Format;
use super::sql;

pub fn query(conn: &Connection, mark_read: bool, format: Format) -> Result<()> {
    if mark_read {
        // Would call `gh api --method PATCH /notifications/threads/{id}` for each unread row.
        // Requires gh client access; not yet implemented.
        anyhow::bail!("--mark-read is not yet implemented");
    }
    sql::query_view(conn, "v_unread_notifications", format)
}
