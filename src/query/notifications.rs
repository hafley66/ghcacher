use anyhow::Result;
use rusqlite::Connection;

use crate::output::Format;
use super::sql;

pub fn query(conn: &Connection, _mark_read: bool, format: Format) -> Result<()> {
    // mark_read would call `gh api --method PATCH /notifications/threads/{id}` for each
    // Not yet implemented -- would need gh client passed in
    if _mark_read {
        tracing::warn!("--mark-read not yet implemented");
    }
    sql::query_view(conn, "v_unread_notifications", format)
}
