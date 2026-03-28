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

CREATE TABLE IF NOT EXISTS pr_requested_reviewer (
    pr_id           INTEGER NOT NULL REFERENCES pull_request(id),
    reviewer        TEXT NOT NULL,
    PRIMARY KEY (pr_id, reviewer)
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
    subject_number  INTEGER,
    html_url        TEXT,
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
    n.gh_id, n.subject_type, n.subject_title, n.subject_number,
    n.html_url, n.reason, n.unread, n.updated_at,
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
