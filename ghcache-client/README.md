# ghcache-client

Read-only Rust client for the [ghcache](../README.md) SQLite database and HTTP command server.

```toml
[dependencies]
ghcache-client = { path = "../ghcache-client" }
```

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

let client = Client::open("/path/to/gh.db".as_ref())?;

// Open PRs needing review
let prs = client.prs(&PrFilter {
    state: Some("open"),
    needs_review: true,
    ..Default::default()
})?;

// Single PR with reviews and comments
let pr = client.pr("myorg/backend", 42)?;
let reviews = client.reviews(pr.unwrap().id)?;

// Unread notifications
let notifs = client.unread_notifications()?;

// Branches
let branches = client.branches(Some("myorg/backend"))?;

// Events
let events = client.events(Some("myorg/backend"), Some("PullRequestEvent"), 50)?;
```

### Polling the change log

`Subscriber` opens its own read-only connection and polls `change_log` on an interval.
Blocks until the handler returns `Err`.

```rust
use ghcache_client::Subscriber;
use std::time::Duration;

Subscriber::new("/path/to/gh.db")
    .interval(Duration::from_millis(500))
    .subscribe(|events| {
        for ev in events {
            println!("{} {} in {}", ev.event, ev.entity_type, ev.repo_slug.as_deref().unwrap_or("?"));
        }
        Ok(())
    })?;
```

`ChangeEvent` fields: `id`, `entity_type` (`pull_request` | `branch` | `notification` | `repo_event`),
`entity_id`, `event` (`inserted` | `updated`), `repo_slug`, `payload` (JSON), `occurred_at`.

To start from the current tail rather than replaying history:

```rust
let client = Client::open(db_path)?;
let last_id = client.latest_change_id()?;
// pass last_id to tail::poll() directly, or use changes_since()
let new_events = client.changes_since(last_id)?;
```

---

## HTTP server integration

`ghcache watch` runs an HTTP server on `127.0.0.1:7748`. Use this when you want:
- The server to clone/fetch a repo into `staging_folder` on demand
- Push delivery via SSE instead of polling
- Pause/resume control over the sync loop

### Register and keep a subscription alive

```rust
use ghcache_client::cmd;

let uuid = "my-process-unique-id";
let port = cmd::DEFAULT_PORT;

// Clone/fetch myorg/backend and register for dynamic PR sync.
// Returns the local path to the bare checkout.
let local_path = cmd::ensure_repo(port, uuid, "myorg", "backend", true, false)?;

// Renew TTL in a background thread (default TTL is 30 seconds).
std::thread::spawn(move || loop {
    std::thread::sleep(std::time::Duration::from_secs(10));
    let _ = cmd::heartbeat(port, uuid);
});
```

### SSE push stream

`EventStream` connects to `GET /events`, sends `Last-Event-ID` to backfill any missed rows,
then streams events as they arrive. Blocks until the connection drops or handler returns `Err`.

```rust
use ghcache_client::EventStream;

EventStream::new(7748)
    .subscribe(|ev| {
        println!("{:?}", ev);
        Ok(())
    })?;

// Resume from a known position (server replays missed rows):
EventStream::from_id(7748, last_seen_id).subscribe(handler)?;
```

### Pause and resume the sync loop

```rust
cmd::pause(7748)?;
// ... do something that needs a stable snapshot ...
cmd::resume(7748)?;
```

---

## Types

| Type | Fields |
|------|--------|
| `PullRequest` | `id`, `repo_slug`, `number`, `title`, `author`, `head_ref`, `base_ref`, `state`, `draft`, `mergeable`, `additions`, `deletions`, `changed_files`, `created_at`, `updated_at`, `merged_at`, `body` |
| `Review` | `id`, `pr_id`, `author`, `state` (`APPROVED` \| `CHANGES_REQUESTED` \| `COMMENTED`), `body`, `submitted_at` |
| `Comment` | `id`, `pr_id`, `author`, `body`, `path`, `line`, `created_at` |
| `Notification` | `id`, `gh_id`, `repo_slug`, `subject_type`, `subject_title`, `subject_url`, `reason`, `unread`, `updated_at` |
| `Branch` | `id`, `repo_slug`, `name`, `sha`, `behind_default`, `ahead_default`, `updated_at` |
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
