use anyhow::{Context, Result};
use rusqlite::{Connection, params};
use std::path::Path;

pub fn open(path: &Path) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating db directory {}", parent.display()))?;
        }
    }
    let conn = Connection::open(path)
        .with_context(|| format!("opening database at {}", path.display()))?;
    configure(&conn)?;
    migrate(&conn)?;
    Ok(conn)
}

pub fn open_in_memory() -> Result<Connection> {
    let conn = Connection::open_in_memory()?;
    configure(&conn)?;
    migrate(&conn)?;
    Ok(conn)
}

fn configure(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA foreign_keys = ON;
         PRAGMA synchronous = NORMAL;
         PRAGMA cache_size = -8000;",
    )?;
    Ok(())
}

fn migrate(conn: &Connection) -> Result<()> {
    conn.execute_batch(SCHEMA)?;
    Ok(())
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS repo (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    owner           TEXT NOT NULL,
    name            TEXT NOT NULL,
    default_branch  TEXT NOT NULL DEFAULT 'main',
    gh_node_id      TEXT,
    updated_at      TEXT,
    UNIQUE(owner, name)
);

CREATE TABLE IF NOT EXISTS branch (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    repo_id         INTEGER NOT NULL REFERENCES repo(id),
    name            TEXT NOT NULL,
    sha             TEXT,
    behind_default  INTEGER,
    ahead_default   INTEGER,
    updated_at      TEXT,
    UNIQUE(repo_id, name)
);

CREATE TABLE IF NOT EXISTS pull_request (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    repo_id         INTEGER NOT NULL REFERENCES repo(id),
    number          INTEGER NOT NULL,
    gh_node_id      TEXT,
    state           TEXT NOT NULL,
    title           TEXT NOT NULL,
    author          TEXT,
    head_ref        TEXT,
    head_sha        TEXT,
    base_ref        TEXT,
    mergeable       TEXT,
    draft           INTEGER NOT NULL DEFAULT 0,
    additions       INTEGER,
    deletions       INTEGER,
    changed_files   INTEGER,
    created_at      TEXT NOT NULL,
    updated_at      TEXT NOT NULL,
    merged_at       TEXT,
    closed_at       TEXT,
    body            TEXT,
    raw_json        TEXT,
    UNIQUE(repo_id, number)
);

CREATE TABLE IF NOT EXISTS pr_review (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    pr_id           INTEGER NOT NULL REFERENCES pull_request(id),
    gh_id           INTEGER NOT NULL,
    author          TEXT,
    state           TEXT NOT NULL,
    body            TEXT,
    submitted_at    TEXT,
    UNIQUE(pr_id, gh_id)
);

CREATE TABLE IF NOT EXISTS pr_comment (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    pr_id           INTEGER NOT NULL REFERENCES pull_request(id),
    gh_id           INTEGER NOT NULL,
    author          TEXT,
    body            TEXT NOT NULL,
    path            TEXT,
    line            INTEGER,
    in_reply_to_id  INTEGER,
    created_at      TEXT NOT NULL,
    updated_at      TEXT NOT NULL,
    UNIQUE(pr_id, gh_id)
);

CREATE TABLE IF NOT EXISTS pr_status_check (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    pr_id           INTEGER NOT NULL REFERENCES pull_request(id),
    context         TEXT NOT NULL,
    state           TEXT NOT NULL,
    target_url      TEXT,
    description     TEXT,
    updated_at      TEXT,
    UNIQUE(pr_id, context)
);

CREATE TABLE IF NOT EXISTS pr_label (
    pr_id           INTEGER NOT NULL REFERENCES pull_request(id),
    label           TEXT NOT NULL,
    color           TEXT,
    PRIMARY KEY (pr_id, label)
);

CREATE TABLE IF NOT EXISTS repo_event (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    repo_id         INTEGER NOT NULL REFERENCES repo(id),
    gh_id           TEXT NOT NULL,
    type            TEXT NOT NULL,
    actor           TEXT,
    payload_json    TEXT NOT NULL,
    created_at      TEXT NOT NULL,
    UNIQUE(repo_id, gh_id)
);

CREATE TABLE IF NOT EXISTS notification (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    gh_id           TEXT NOT NULL UNIQUE,
    repo_id         INTEGER REFERENCES repo(id),
    subject_type    TEXT NOT NULL,
    subject_title   TEXT NOT NULL,
    subject_url     TEXT,
    reason          TEXT NOT NULL,
    unread          INTEGER NOT NULL DEFAULT 1,
    updated_at      TEXT NOT NULL,
    last_read_at    TEXT
);

CREATE TABLE IF NOT EXISTS call_log (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    endpoint        TEXT NOT NULL,
    api_type        TEXT NOT NULL DEFAULT 'rest',   -- rest | graphql
    method          TEXT NOT NULL DEFAULT 'GET',
    status_code     INTEGER,
    etag            TEXT,
    last_modified   TEXT,
    rate_remaining  INTEGER,
    rate_reset      INTEGER,
    gql_cost        INTEGER,                        -- GraphQL point cost for this query
    cache_hit       INTEGER NOT NULL DEFAULT 0,
    duration_ms     INTEGER,
    called_at       TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
);

-- Append-only log of every entity change during sync.
-- Consumers tail this with: SELECT * FROM change_log WHERE id > :last_seen
CREATE TABLE IF NOT EXISTS change_log (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    entity_type TEXT NOT NULL,   -- pull_request | notification | branch | repo_event
    entity_id   INTEGER NOT NULL,
    event       TEXT NOT NULL,   -- inserted | updated
    repo_slug   TEXT,
    payload_json TEXT,
    occurred_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
);

CREATE TABLE IF NOT EXISTS poll_state (
    endpoint        TEXT PRIMARY KEY,
    etag            TEXT,
    last_modified   TEXT,
    poll_interval   INTEGER,
    last_polled_at  TEXT,
    last_changed_at TEXT
);

CREATE TABLE IF NOT EXISTS checkout (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    repo_id         INTEGER NOT NULL REFERENCES repo(id),
    branch          TEXT NOT NULL,
    local_path      TEXT NOT NULL,
    sha             TEXT,
    checked_out_at  TEXT NOT NULL,
    UNIQUE(repo_id, branch)
);

CREATE INDEX IF NOT EXISTS idx_pr_repo_state   ON pull_request(repo_id, state);
CREATE INDEX IF NOT EXISTS idx_pr_updated      ON pull_request(updated_at);
CREATE INDEX IF NOT EXISTS idx_event_repo_type ON repo_event(repo_id, type);
CREATE INDEX IF NOT EXISTS idx_event_created   ON repo_event(created_at);
CREATE INDEX IF NOT EXISTS idx_notif_unread    ON notification(unread, updated_at);
CREATE INDEX IF NOT EXISTS idx_call_endpoint   ON call_log(endpoint, called_at);
CREATE INDEX IF NOT EXISTS idx_change_log      ON change_log(entity_type, occurred_at);
CREATE INDEX IF NOT EXISTS idx_branch_repo     ON branch(repo_id);

CREATE VIEW IF NOT EXISTS v_open_prs AS
SELECT
    r.owner || '/' || r.name AS repo_slug,
    pr.number, pr.title, pr.author, pr.head_ref,
    pr.draft, pr.mergeable, pr.additions, pr.deletions,
    pr.created_at, pr.updated_at,
    (SELECT COUNT(*) FROM pr_review rv WHERE rv.pr_id = pr.id AND rv.state = 'APPROVED') AS approvals,
    (SELECT COUNT(*) FROM pr_review rv WHERE rv.pr_id = pr.id AND rv.state = 'CHANGES_REQUESTED') AS changes_requested,
    (SELECT COUNT(*) FROM pr_comment c WHERE c.pr_id = pr.id) AS comment_count
FROM pull_request pr
JOIN repo r ON r.id = pr.repo_id
WHERE pr.state = 'open';

CREATE VIEW IF NOT EXISTS v_unread_notifications AS
SELECT
    n.gh_id, n.subject_type, n.subject_title, n.reason,
    n.updated_at,
    r.owner || '/' || r.name AS repo_slug
FROM notification n
LEFT JOIN repo r ON r.id = n.repo_id
WHERE n.unread = 1
ORDER BY n.updated_at DESC;

CREATE VIEW IF NOT EXISTS v_recent_events AS
SELECT
    r.owner || '/' || r.name AS repo_slug,
    e.type, e.actor, e.created_at,
    json_extract(e.payload_json, '$.action') AS action,
    CASE e.type
        WHEN 'PullRequestEvent' THEN json_extract(e.payload_json, '$.pull_request.title')
        WHEN 'PushEvent'        THEN json_extract(e.payload_json, '$.ref')
        WHEN 'IssuesEvent'      THEN json_extract(e.payload_json, '$.issue.title')
        ELSE NULL
    END AS subject
FROM repo_event e
JOIN repo r ON r.id = e.repo_id
ORDER BY e.created_at DESC
LIMIT 100;

CREATE VIEW IF NOT EXISTS v_rate_limit AS
SELECT
    endpoint,
    status_code,
    cache_hit,
    rate_remaining,
    datetime(rate_reset, 'unixepoch') AS rate_resets_at,
    duration_ms,
    called_at
FROM call_log
ORDER BY called_at DESC
LIMIT 50;
";

// ---- repo upsert -------------------------------------------------------

pub fn upsert_repo(
    conn: &Connection,
    owner: &str,
    name: &str,
    default_branch: &str,
) -> Result<i64> {
    conn.execute(
        "INSERT INTO repo (owner, name, default_branch)
         VALUES (?1, ?2, ?3)
         ON CONFLICT(owner, name) DO UPDATE SET
             default_branch = excluded.default_branch",
        params![owner, name, default_branch],
    )?;
    let id = conn.query_row(
        "SELECT id FROM repo WHERE owner = ?1 AND name = ?2",
        params![owner, name],
        |row| row.get(0),
    )?;
    Ok(id)
}

pub fn get_repo_id(conn: &Connection, owner: &str, name: &str) -> Result<Option<i64>> {
    let mut stmt = conn.prepare_cached("SELECT id FROM repo WHERE owner = ?1 AND name = ?2")?;
    let mut rows = stmt.query(params![owner, name])?;
    Ok(rows.next()?.map(|r| r.get_unwrap(0)))
}

// ---- poll_state --------------------------------------------------------

pub struct PollState {
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub poll_interval: Option<i64>,
}

pub fn get_poll_state(conn: &Connection, endpoint: &str) -> Result<PollState> {
    let mut stmt = conn.prepare_cached(
        "SELECT etag, last_modified, poll_interval FROM poll_state WHERE endpoint = ?1",
    )?;
    let mut rows = stmt.query(params![endpoint])?;
    if let Some(row) = rows.next()? {
        Ok(PollState {
            etag: row.get(0)?,
            last_modified: row.get(1)?,
            poll_interval: row.get(2)?,
        })
    } else {
        Ok(PollState { etag: None, last_modified: None, poll_interval: None })
    }
}

pub fn set_poll_state(
    conn: &Connection,
    endpoint: &str,
    etag: Option<&str>,
    last_modified: Option<&str>,
    poll_interval: Option<i64>,
    changed: bool,
) -> Result<()> {
    let now = chrono::Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO poll_state (endpoint, etag, last_modified, poll_interval, last_polled_at, last_changed_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(endpoint) DO UPDATE SET
             etag            = excluded.etag,
             last_modified   = excluded.last_modified,
             poll_interval   = COALESCE(excluded.poll_interval, poll_state.poll_interval),
             last_polled_at  = excluded.last_polled_at,
             last_changed_at = CASE WHEN ?6 IS NOT NULL THEN excluded.last_changed_at ELSE poll_state.last_changed_at END",
        params![
            endpoint,
            etag,
            last_modified,
            poll_interval,
            now,
            if changed { Some(now.as_str()) } else { None },
        ],
    )?;
    Ok(())
}

// ---- call_log ----------------------------------------------------------

pub struct CallLogEntry<'a> {
    pub endpoint: &'a str,
    pub api_type: &'a str,   // "rest" | "graphql"
    pub method: &'a str,
    pub status_code: Option<u16>,
    pub etag: Option<&'a str>,
    pub last_modified: Option<&'a str>,
    pub rate_remaining: Option<i64>,
    pub rate_reset: Option<i64>,
    pub gql_cost: Option<i64>,
    pub cache_hit: bool,
    pub duration_ms: Option<i64>,
}

pub fn log_call(conn: &Connection, entry: &CallLogEntry) -> Result<()> {
    conn.execute(
        "INSERT INTO call_log
         (endpoint, api_type, method, status_code, etag, last_modified,
          rate_remaining, rate_reset, gql_cost, cache_hit, duration_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            entry.endpoint,
            entry.api_type,
            entry.method,
            entry.status_code.map(|c| c as i64),
            entry.etag,
            entry.last_modified,
            entry.rate_remaining,
            entry.rate_reset,
            entry.gql_cost,
            entry.cache_hit as i64,
            entry.duration_ms,
        ],
    )?;
    Ok(())
}

// ---- change_log --------------------------------------------------------

pub enum ChangeEvent {
    Inserted,
    Updated,
}

impl ChangeEvent {
    fn as_str(&self) -> &'static str {
        match self {
            ChangeEvent::Inserted => "inserted",
            ChangeEvent::Updated => "updated",
        }
    }
}

pub fn log_change(
    conn: &Connection,
    entity_type: &str,
    entity_id: i64,
    event: ChangeEvent,
    repo_slug: Option<&str>,
    payload: Option<&serde_json::Value>,
) -> Result<()> {
    let payload_str = payload.map(|p| serde_json::to_string(p)).transpose()?;
    conn.execute(
        "INSERT INTO change_log (entity_type, entity_id, event, repo_slug, payload_json)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![entity_type, entity_id, event.as_str(), repo_slug, payload_str],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_creates_without_error() {
        open_in_memory().unwrap();
    }

    #[test]
    fn upsert_repo_idempotent() {
        let conn = open_in_memory().unwrap();
        let id1 = upsert_repo(&conn, "myorg", "backend", "main").unwrap();
        let id2 = upsert_repo(&conn, "myorg", "backend", "main").unwrap();
        assert_eq!(id1, id2);
    }

    #[test]
    fn upsert_repo_updates_default_branch() {
        let conn = open_in_memory().unwrap();
        upsert_repo(&conn, "myorg", "backend", "master").unwrap();
        upsert_repo(&conn, "myorg", "backend", "main").unwrap();
        let branch: String = conn
            .query_row(
                "SELECT default_branch FROM repo WHERE owner='myorg' AND name='backend'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(branch, "main");
    }

    #[test]
    fn get_repo_id_missing_returns_none() {
        let conn = open_in_memory().unwrap();
        assert!(get_repo_id(&conn, "x", "y").unwrap().is_none());
    }

    #[test]
    fn get_repo_id_after_upsert() {
        let conn = open_in_memory().unwrap();
        let id = upsert_repo(&conn, "o", "n", "main").unwrap();
        assert_eq!(get_repo_id(&conn, "o", "n").unwrap(), Some(id));
    }

    #[test]
    fn poll_state_roundtrip() {
        let conn = open_in_memory().unwrap();
        set_poll_state(&conn, "/repos/o/n/events", Some("\"abc\""), None, Some(60), true).unwrap();
        let ps = get_poll_state(&conn, "/repos/o/n/events").unwrap();
        assert_eq!(ps.etag.as_deref(), Some("\"abc\""));
        assert_eq!(ps.poll_interval, Some(60));
    }

    #[test]
    fn poll_state_preserves_interval_on_304() {
        let conn = open_in_memory().unwrap();
        set_poll_state(&conn, "/ep", None, None, Some(120), true).unwrap();
        // 304: pass None for poll_interval
        set_poll_state(&conn, "/ep", None, None, None, false).unwrap();
        let ps = get_poll_state(&conn, "/ep").unwrap();
        assert_eq!(ps.poll_interval, Some(120));
    }

    #[test]
    fn call_log_insert() {
        let conn = open_in_memory().unwrap();
        log_call(
            &conn,
            &CallLogEntry {
                endpoint: "/repos/o/n/pulls",
                api_type: "rest",
                method: "GET",
                status_code: Some(200),
                etag: Some("\"xyz\""),
                last_modified: None,
                rate_remaining: Some(4999),
                rate_reset: Some(1700000000),
                gql_cost: None,
                cache_hit: false,
                duration_ms: Some(142),
            },
        )
        .unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM call_log", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn foreign_key_enforced() {
        let conn = open_in_memory().unwrap();
        let err = conn.execute(
            "INSERT INTO branch (repo_id, name) VALUES (999, 'main')",
            [],
        );
        assert!(err.is_err());
    }

    #[test]
    fn views_exist() {
        let conn = open_in_memory().unwrap();
        // Just verify the views can be queried without error
        conn.execute_batch(
            "SELECT * FROM v_open_prs LIMIT 1;
             SELECT * FROM v_unread_notifications LIMIT 1;
             SELECT * FROM v_recent_events LIMIT 1;
             SELECT * FROM v_rate_limit LIMIT 1;",
        )
        .unwrap();
    }

    #[test]
    fn change_log_insert() {
        let conn = open_in_memory().unwrap();
        let repo_id = upsert_repo(&conn, "o", "n", "main").unwrap();
        log_change(&conn, "pull_request", repo_id, ChangeEvent::Inserted, Some("o/n"), None).unwrap();
        log_change(&conn, "pull_request", repo_id, ChangeEvent::Updated, Some("o/n"), None).unwrap();

        let rows: Vec<(String, String)> = {
            let mut stmt = conn.prepare("SELECT entity_type, event FROM change_log ORDER BY id").unwrap();
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .unwrap()
                .filter_map(|r| r.ok())
                .collect()
        };
        assert_eq!(rows, vec![
            ("pull_request".into(), "inserted".into()),
            ("pull_request".into(), "updated".into()),
        ]);
    }

    #[test]
    fn change_log_tail_pattern() {
        let conn = open_in_memory().unwrap();
        log_change(&conn, "notification", 1, ChangeEvent::Inserted, None, None).unwrap();
        log_change(&conn, "notification", 2, ChangeEvent::Inserted, None, None).unwrap();
        log_change(&conn, "notification", 3, ChangeEvent::Updated, None, None).unwrap();

        let ids: Vec<i64> = {
            let mut stmt = conn.prepare("SELECT entity_id FROM change_log WHERE id > 1 ORDER BY id").unwrap();
            stmt.query_map([], |r| r.get(0))
                .unwrap()
                .filter_map(|r| r.ok())
                .collect()
        };
        assert_eq!(ids, vec![2, 3]);
    }

    #[test]
    fn change_log_with_payload() {
        let conn = open_in_memory().unwrap();
        let payload = serde_json::json!({ "number": 42, "title": "My PR" });
        log_change(&conn, "pull_request", 42, ChangeEvent::Inserted, Some("o/n"), Some(&payload)).unwrap();

        let stored: Option<String> = conn
            .query_row("SELECT payload_json FROM change_log WHERE entity_id=42", [], |r| r.get(0))
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&stored.unwrap()).unwrap();
        assert_eq!(v["number"], 42);
    }
}
