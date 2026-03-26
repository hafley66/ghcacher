# ghcache-client

Read-only Rust client for the [ghcache](../README.md) SQLite database and HTTP command server.

```toml
[dependencies]
ghcache-client = { path = "../ghcache-client" }
```

## Finding the database

The ghcache CLI resolves `db_path` from config (default `~/.local/share/ghcache/gh.db`).
To get the resolved path from another tool:

```bash
# Print the path the running server is using
ghcache db-path

# Use it with sqlite3 directly
sqlite3 "$(ghcache db-path)"

# Use it in another process
export GHCACHE_DB="$(ghcache db-path)"
```

Config resolution order: `--config <path>` flag, `$GHCACHE_CONFIG` env var, `./ghcache.toml`, `~/.config/ghcache/config.toml`. The `db_path` field in `[global]` controls the location.

## Querying the database directly

The SQLite database is WAL-mode, so concurrent reads from external tools are safe while ghcache is running.

### From the CLI

```bash
# Raw SQL passthrough -- any SELECT works
ghcache query sql "SELECT owner, name, default_branch FROM repo"

# Output formats: --format table (default), json, json-pretty, tsv
ghcache query sql "SELECT * FROM branch WHERE sha IS NOT NULL" --format json

# Built-in views for common queries
ghcache query sql "SELECT * FROM v_open_prs"
ghcache query sql "SELECT * FROM v_unread_notifications"
ghcache query sql "SELECT * FROM v_recent_events"
ghcache query sql "SELECT * FROM v_rate_limit"
```

### From sqlite3

```bash
sqlite3 "$(ghcache db-path)" "SELECT * FROM checkout"
```

### Schema overview

Core tables: `repo`, `branch`, `pull_request`, `pr_review`, `pr_comment`, `pr_status_check`, `pr_label`, `repo_event`, `notification`, `call_log`, `change_log`, `poll_state`, `checkout`.

The `change_log` table is append-only and designed for tailing:

```sql
SELECT * FROM change_log WHERE id > :last_seen ORDER BY id
```

Each row has `entity_type` (`pull_request` | `branch` | `notification` | `repo_event`), `entity_id`, `event` (`inserted` | `updated`), `repo_slug`, and `payload_json`.

## Two delivery modes

| Mode | When to use |
|------|-------------|
| `Client` + `Subscriber` | Direct DB access -- same machine, no network |
| `EventStream` + `cmd` | HTTP server integration -- register subscription, get push events |

Both can coexist in the same process.

---

## Direct DB access

### Point-in-time queries

```rust
use ghcache_client::{Client, PrFilter};

let client = Client::open("/path/to/gh.db".as_ref()).await?;

// Open PRs needing review
let prs = client.prs(&PrFilter {
    state: Some("open"),
    needs_review: true,
    ..Default::default()
}).await?;

// Single PR with reviews and comments
let pr = client.pr("myorg/backend", 42).await?;
let reviews = client.reviews(pr.unwrap().id).await?;

// Unread notifications
let notifs = client.unread_notifications().await?;

// Branches with their current SHA
let branches = client.branches(Some("myorg/backend")).await?;

// Checkouts -- on-disk paths for each checked-out branch
let checkouts = client.checkouts(None).await?;          // all repos
let checkouts = client.checkouts(Some("myorg/backend")).await?; // one repo
for co in &checkouts {
    println!("{} {} -> {} ({})", co.repo_slug, co.branch, co.local_path, co.sha.as_deref().unwrap_or("?"));
}

// Events
let events = client.events(Some("myorg/backend"), Some("PullRequestEvent"), 50).await?;

// Rate limit state
let limits = client.rate_limit().await?;
```

### Polling the change log

`Subscriber` opens its own read-only connection and polls `change_log` on an interval.
Blocks until the handler returns `Err`.

```rust
use ghcache_client::Subscriber;
use std::time::Duration;

Subscriber::new("/path/to/gh.db")
    .interval(Duration::from_millis(500))
    .subscribe(|events| async move {
        for ev in &events {
            println!("{} {} in {}", ev.event, ev.entity_type, ev.repo_slug.as_deref().unwrap_or("?"));
        }
        Ok(())
    })
    .await?;
```

`ChangeEvent` fields: `id`, `entity_type` (`pull_request` | `branch` | `notification` | `repo_event`),
`entity_id`, `event` (`inserted` | `updated`), `repo_slug`, `payload` (JSON), `occurred_at`.

To start from the current tail rather than replaying history:

```rust
let client = Client::open(db_path).await?;
let last_id = client.latest_change_id().await?;
// pass last_id to tail::poll() directly, or use changes_since()
let new_events = client.changes_since(last_id).await?;
```

---

## HTTP server integration

`ghcache watch` runs an HTTP server on `127.0.0.1:7748` (configurable via `cmd_port` in `[global]`).

### HTTP API reference

All endpoints are on `127.0.0.1:{cmd_port}`. JSON request/response bodies.

#### `POST /subscribe` -- register for repo sync

Clones the repo into `staging_folder` if not present, fetches latest, and registers the caller as a subscriber. The subscription expires after `heartbeat_ttl_seconds` (default 30) unless renewed.

```bash
curl -X POST http://127.0.0.1:7748/subscribe \
  -H 'Content-Type: application/json' \
  -d '{"uuid":"my-tool-1","owner":"myorg","repo":"backend","pr_sync":true,"notifications":false}'
# => {"path":"/home/user/.ghcache/staging/myorg/backend"}
```

| Field | Type | Description |
|---|---|---|
| `uuid` | string | Unique caller ID. Same UUID can subscribe to multiple repos. |
| `owner` | string | GitHub org or user |
| `repo` | string | Repository name |
| `pr_sync` | bool | If true, ghcache runs the full PR/review/comment sync for this repo while subscription is alive |
| `notifications` | bool | If true, ghcache syncs the authenticated user's notification inbox |

Response: `{"path": "<local clone path>"}`

#### `POST /heartbeat` -- renew subscription TTL

```bash
curl -X POST http://127.0.0.1:7748/heartbeat \
  -H 'Content-Type: application/json' \
  -d '{"uuid":"my-tool-1"}'
# => {"ok":true}
```

Returns `{"ok":false}` if the UUID has already expired.

#### `POST /pause` -- pause the sync loop

```bash
curl -X POST http://127.0.0.1:7748/pause -d '{}'
# => {"ok":true}
```

#### `POST /resume` -- resume after pause

```bash
curl -X POST http://127.0.0.1:7748/resume -d '{}'
# => {"ok":true}
```

#### `GET /events` -- SSE change stream

Server-Sent Events stream of every change_log row. Send `Last-Event-ID` header to replay missed events from that point.

```bash
# Stream from now
curl -N http://127.0.0.1:7748/events

# Resume from last seen ID
curl -N -H 'Last-Event-ID: 42' http://127.0.0.1:7748/events
```

Each SSE frame:

```
id: 43
data: {"id":43,"entity_type":"branch","entity_id":7,"event":"updated","repo_slug":"myorg/backend","payload":null,"occurred_at":"2026-03-27T00:00:00Z"}
```

The server polls change_log every 500ms and pushes to all connected clients. Connection is kept alive indefinitely.

### Rust client wrappers

The `ghcache_client` crate wraps the HTTP endpoints above:

```rust
use ghcache_client::cmd;

let uuid = "my-process-unique-id";
let port = cmd::DEFAULT_PORT;

// Subscribe -- clones/fetches and registers for sync
let local_path = cmd::ensure_repo(port, uuid, "myorg", "backend", true, false).await?;

// Keep alive in background (renew before TTL expires)
tokio::spawn(async move {
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(10)).await;
        let _ = cmd::heartbeat(port, uuid).await;
    }
});

// Pause/resume
cmd::pause(7748).await?;
cmd::resume(7748).await?;
```

### SSE from Rust

```rust
use ghcache_client::EventStream;

EventStream::new(7748)
    .subscribe(|ev| async move {
        println!("{:?}", ev);
        Ok(())
    })
    .await?;

// Resume from a known position (server replays missed rows):
EventStream::from_id(7748, last_seen_id).subscribe(handler).await?;
```

---

## SQLite tables

The database is WAL-mode SQLite. Safe for concurrent reads from external processes while ghcache is running.

| Table | Key columns | Notes |
|---|---|---|
| `repo` | `id`, `owner`, `name`, `default_branch` | UNIQUE(owner, name) |
| `branch` | `id`, `repo_id`, `name`, `sha`, `behind_default`, `ahead_default` | UNIQUE(repo_id, name) |
| `pull_request` | `id`, `repo_id`, `number`, `state`, `title`, `author`, `head_ref`, `head_sha`, `base_ref`, `draft`, `mergeable`, `additions`, `deletions`, `body`, `raw_json` | UNIQUE(repo_id, number) |
| `pr_review` | `id`, `pr_id`, `gh_id`, `author`, `state`, `body` | UNIQUE(pr_id, gh_id) |
| `pr_comment` | `id`, `pr_id`, `gh_id`, `author`, `body`, `path`, `line` | UNIQUE(pr_id, gh_id) |
| `pr_status_check` | `id`, `pr_id`, `context`, `state`, `target_url` | UNIQUE(pr_id, context) |
| `pr_label` | `pr_id`, `label`, `color` | PRIMARY KEY(pr_id, label) |
| `repo_event` | `id`, `repo_id`, `gh_id`, `type`, `actor`, `payload_json` | UNIQUE(repo_id, gh_id) |
| `notification` | `id`, `gh_id`, `repo_id`, `subject_type`, `subject_title`, `subject_number`, `html_url`, `reason`, `unread` | UNIQUE(gh_id) |
| `checkout` | `id`, `repo_id`, `branch`, `local_path`, `sha` | UNIQUE(repo_id, branch) |
| `call_log` | `id`, `endpoint`, `api_type`, `status_code`, `rate_remaining`, `rate_reset`, `cache_hit`, `duration_ms` | Append-only API call history |
| `change_log` | `id`, `entity_type`, `entity_id`, `event`, `repo_slug`, `payload_json`, `occurred_at` | Append-only, tail with `WHERE id > :last_seen` |
| `poll_state` | `endpoint`, `etag`, `last_modified`, `poll_interval`, `last_polled_at` | PRIMARY KEY(endpoint) |

### Built-in views

| View | Description |
|---|---|
| `v_open_prs` | Open PRs with approval/comment counts |
| `v_unread_notifications` | Unread notifications with repo slug |
| `v_recent_events` | Last 100 events with extracted action/subject |
| `v_rate_limit` | Last 50 API calls with rate limit state |

```bash
sqlite3 "$(ghcache db-path)" "SELECT * FROM v_open_prs"
ghcache query sql "SELECT * FROM v_unread_notifications" --format json
```

---

## Types

| Type | Fields |
|------|--------|
| `PullRequest` | `id`, `repo_slug`, `number`, `title`, `author`, `head_ref`, `base_ref`, `state`, `draft`, `mergeable`, `additions`, `deletions`, `changed_files`, `created_at`, `updated_at`, `merged_at`, `body` |
| `Review` | `id`, `pr_id`, `author`, `state` (`APPROVED` \| `CHANGES_REQUESTED` \| `COMMENTED`), `body`, `submitted_at` |
| `Comment` | `id`, `pr_id`, `author`, `body`, `path`, `line`, `created_at` |
| `Notification` | `id`, `gh_id`, `repo_slug`, `subject_type`, `subject_title`, `subject_url`, `subject_number` (PR/issue number), `html_url` (browser URL), `reason`, `unread`, `updated_at` |
| `Branch` | `id`, `repo_slug`, `name`, `sha`, `behind_default`, `ahead_default`, `updated_at` |
| `Checkout` | `repo_slug`, `branch`, `local_path`, `sha`, `checked_out_at` |
| `RepoEvent` | `id`, `repo_slug`, `gh_id`, `event_type`, `actor`, `payload` (JSON), `created_at` |
| `RateLimit` | `api_type` (`rest` \| `graphql`), `remaining`, `reset_at`, `gql_cost` |
| `ChangeEvent` | `id`, `entity_type`, `entity_id`, `event`, `repo_slug`, `payload` (JSON), `occurred_at` |
| `PrFilter<'a>` | `repo`, `state`, `needs_review`, `author` -- all optional, passed to `Client::prs()` |

All types implement `Debug`, `Clone`, `Serialize`, `Deserialize`.

---

## Choosing Subscriber vs EventStream

- **`Subscriber`** (polling): no server required, works any time the DB is on disk. Slightly higher latency (poll interval). Good default.
- **`EventStream`** (SSE): requires `ghcache watch` to be running. Zero-latency push. Also triggers `ensure_repo` checkout via `cmd::subscribe`, so the server keeps your branch up to date.

Both handle back-pressure the same way: if the handler is slow, events queue in memory between polls/lines.
