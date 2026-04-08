# ghcache

A local caching proxy between the `gh` CLI and a normalized SQLite database.
GitHub data is synced into SQLite; local tools query SQLite directly -- no live API calls at read time.

## Quick start

```sh
# Option A: guided TUI
ghcache setup

# Option B: manual
ghcache init          # write ghcache.toml template
$EDITOR ghcache.toml  # edit owner/repos
ghcache sync          # full initial sync
ghcache watch         # continuous polling loop
```

## Setup (guided TUI)

`ghcache setup` runs interactive terminal prompts, writes `ghcache.toml`, and runs the initial sync.

```sh
ghcache setup

# Write the config to a custom path:
ghcache --config ~/.config/ghcache/config.toml setup
```

The `ghcache-init` companion binary (Go + Charm huh) provides a richer multi-step form as an
alternative. It is not required -- setup works without it.

```sh
cd init-form
go build -o ghcache-init .
cp ghcache-init /usr/local/bin/   # or next to the ghcache binary
```

`ghcache-init` outputs JSON to stdout; pipe it wherever you like, or use it standalone to
generate config JSON for scripted setups.

## Demo (copy-paste)

`demo.sh` in the repo root runs the full demo automatically (builds the binary, writes a temp config
for an org, syncs, queries, and confirms 304 cache hits on the second pass). Just run it:

```sh
./demo.sh
```

Or step through it manually. Replace `yourname` with your GitHub login (`gh api user --jq '.login'`).

**1. Write a throw-away config**

To sync every repo under your account (replace `yourname`):

```sh
cat > /tmp/ghcache-demo.toml << 'EOF'
[global]
db_path   = "/tmp/ghcache-demo.db"
log_level = "warn"
gh_binary = "gh"

[[org]]
owner         = "yourname"
sync_prs      = true
sync_events   = true
sync_branches = ["main"]
exclude       = []          # repo names to skip
EOF
```

Or to target specific repos instead:

```sh
cat > /tmp/ghcache-demo.toml << 'EOF'
[global]
db_path   = "/tmp/ghcache-demo.db"
log_level = "warn"
gh_binary = "gh"

[[repo]]
owner          = "yourname"
name           = "your-repo"
default_branch = "main"
sync_prs       = true
sync_events    = true
sync_branches  = ["main"]
EOF
```

**2. First sync -- full sweep**

```sh
ghcache --config /tmp/ghcache-demo.toml sync
```

**3. Query the DB directly -- no API calls**

```sh
DB=/tmp/ghcache-demo.db

# branches with SHAs
sqlite3 $DB -column -header "
  SELECT r.owner||'/'||r.name AS repo, b.name AS branch, substr(b.sha,1,8) AS sha
  FROM branch b JOIN repo r ON r.id=b.repo_id;"

# open PRs
sqlite3 $DB -column -header "SELECT * FROM v_open_prs;"

# recent events
sqlite3 $DB -column -header "SELECT repo_slug, type, actor, created_at FROM v_recent_events LIMIT 10;"
```

**4. Second sync -- watch it be instant**

```sh
time ghcache --config /tmp/ghcache-demo.toml sync
```

**5. Confirm it was free (304 cache hits)**

```sh
sqlite3 $DB -column -header "
  SELECT endpoint, status_code, cache_hit, duration_ms
  FROM call_log ORDER BY id DESC LIMIT 15;"
```

**6. Clean up**

```sh
rm /tmp/ghcache-demo.toml /tmp/ghcache-demo.db
```

## Config file search order

1. `--config <path>` flag (highest priority)
2. `$GHCACHE_CONFIG` environment variable
3. `./ghcache.toml` (current directory)
4. `~/.config/ghcache/config.toml`

## Config format

```toml
[global]
db_path              = "~/.local/share/ghcache/gh.db"
staging_folder       = "~/.local/share/ghcache/repos"  # git checkouts live here, next to the DB
poll_interval_seconds = 60
log_level            = "info"            # trace | debug | info | warn | error
gh_binary            = "gh"
sync_notifications   = false             # sync the authenticated user's personal notification inbox

# Sync every repo under a GitHub user or org account.
# Discovered at sync time via the GitHub repos API (ETag-gated, usually a free 304).
[[org]]
owner                = "myorg"
sync_prs             = true
sync_notifications   = true
sync_events          = true
sync_branches        = ["main"]
checkout_on_sync     = false
poll_interval_seconds = 60
exclude              = ["archived-repo", "scratch"]   # repo names to skip

# Or target specific repos explicitly.
[[repo]]
owner                = "myorg"
name                 = "backend"
default_branch       = "main"
sync_prs             = true
sync_notifications   = true
sync_events          = true
sync_branches        = ["main", "staging", "release/*"]  # glob trailing * supported
checkout_on_sync     = false   # if true, watch loop runs checkout for sync_branches
poll_interval_seconds = 30    # overrides global for this repo
```

`[[org]]` and `[[repo]]` can coexist. Repos discovered via `[[org]]` that are also listed under `[[repo]]` will be synced twice (once with each config). Use `exclude` to avoid that.

## Global flags

| Flag | Short | Description |
|------|-------|-------------|
| `--config <path>` | | Override config file location |
| `--silent` | `-s` | Suppress JSON rate-limit summary lines on stdout |
| `--calls-to-stdout` | | Print each API call as a JSON line on stdout |

## Stdout JSON lines

Two independent streams, both on stdout, both in JSON Lines format (one object per line).

### Rate-limit summary (`--silent` suppresses)

After every API call, the current pool sizes are emitted:

```json
{"ts":"2026-03-25T10:00:00Z","rest_remaining":4800,"graphql_remaining":4950}
```

Both pools are always shown together (latest recorded value for each).

### Per-call detail (`--calls-to-stdout` enables)

Each individual API call emits its own line with the endpoint and cost:

REST:
```json
{"ts":"2026-03-25T10:00:00Z","endpoint":"/repos/myorg/backend/events","status":200,"rate_remaining":4800,"duration_ms":42}
```

GraphQL:
```json
{"ts":"2026-03-25T10:00:00Z","endpoint":"graphql:prs:full/myorg/backend","status":200,"rate_remaining":4950,"gql_cost":1,"duration_ms":150}
```

`gql_cost` is the GraphQL point cost reported by the GitHub API (null for REST calls).
This is how you see which query caused a rate-limit drop.

Pipe either stream through `jq` to filter:
```sh
ghcache --calls-to-stdout sync | jq 'select(.api_type == "graphql")'
```

## Commands

### `ghcache init`
Writes a `ghcache.toml` template to `~/.config/ghcache/config.toml` (creating the directory if needed)
and prints next steps. Falls back to `./ghcache.toml` if `dirs::config_dir()` is unavailable.

### `ghcache sync [--repo owner/name] [--prs] [--notifications] [--events]`
Full sweep sync: fetches all open PRs, events, notifications, and branches for every configured repo.

- `--repo owner/name` restricts to one repo
- `--prs / --notifications / --events` restrict to one data type
- Always performs a full GraphQL PR fetch (not event-targeted)

### `ghcache watch [--no-sync]`
Continuous polling loop. First iteration is a full sweep; subsequent iterations are event-targeted
(only PRs mentioned in PullRequestEvent / PullRequestReviewEvent payloads are re-fetched via GraphQL).

Runs `checkout` for repos with `checkout_on_sync = true` after each sync pass.

Also starts the HTTP command server on `127.0.0.1:{cmd_port}` (see below). Pass `--no-sync` to run
the HTTP server only, with no sync loop.

`--daemon` double-forks to background and redirects stdio to `/dev/null`. A pidfile is written
next to the database (`gh.pid`) on every `watch` start -- daemon or not -- and checked to prevent
duplicate instances. A stale pidfile (dead process) is silently overwritten.

### `ghcache checkout <owner/name> <branch>`
Clone or update a single branch into `staging_folder`.

Checkout path convention: `{staging_folder}/{owner}/{name}`

Examples:
- `myorg/backend` branch `main`  → `~/src/staging/myorg/backend`
- `myorg/backend` branch `staging` → `~/src/staging/myorg/backend` (same dir, different branch reset)

First checkout: `gh repo clone owner/name dest -- --branch branch` (full history).
Subsequent checkouts: `git fetch origin branch && git reset --hard FETCH_HEAD`
(only runs if `branch.sha` in the DB differs from the last recorded checkout sha).

Requires `staging_folder` in config. Requires the repo to exist in the DB (run `ghcache sync` first).

### `ghcache query <subcommand>`
Query cached data from SQLite without hitting the GitHub API.

| Subcommand | Key flags | Notes |
|------------|-----------|-------|
| `prs` | `--repo owner/name`, `--state open\|closed\|merged`, `--needs-review`, `--mine` | `--mine` resolves current user via `gh api user` |
| `pr <number> --repo owner/name` | | Single PR with reviews, comments, labels |
| `notifications` | `--all`, `--repo owner/name`, `--reason <reason>`, `--type <type>`, `--mark-read` | Default: unread only. `--mark-read` not yet implemented. |
| `events` | `--repo owner/name`, `--type <type>` | |
| `branches` | `--repo owner/name` | |
| `rate-limit` | | Last 50 API calls with rate info |
| `sql '<query>'` | | Raw SQL passthrough |

All subcommands accept `--format json\|json-pretty\|tsv\|table` (default: table on TTY, JSON when piped).

### `ghcache status`
Shows DB size, PR count, unread notification count, and recent poll state per endpoint.

### `ghcache config`
Prints the resolved config values (after tilde expansion, env var overrides, etc.).

### `ghcache db-path`
Prints the resolved database path. Useful for `sqlite3 $(ghcache db-path)`.

## HTTP command server

`ghcache watch` (and `ghcache watch --no-sync`) binds an HTTP server on `127.0.0.1:7748`
(configurable via `cmd_port` in `[global]`). Consumer processes use this to:

- **Register interest** in a repo and get its local path for direct `git2` reads
- **Receive live change events** via SSE as `change_log` rows are written
- **Pause / resume** the sync loop without restarting

### Endpoints

| Method | Path | Body | Description |
|--------|------|------|-------------|
| `GET` | `/events` | — | SSE stream of `change_log` rows. Send `Last-Event-ID: N` header to backfill from row N. |
| `POST` | `/subscribe` | `{"uuid":"…","owner":"…","repo":"…","pr_sync":true,"notifications":false}` | Clone/fetch the repo into `staging_folder`, return its local path, register it for dynamic sync. |
| `POST` | `/heartbeat` | `{"uuid":"…"}` | Refresh TTL for a subscription. Returns `{"ok":true/false}`. |
| `POST` | `/pause` | — | Pause the sync loop. |
| `POST` | `/resume` | — | Resume the sync loop. |

Subscriptions expire after `heartbeat_ttl_seconds` (default 30) without a heartbeat.
Active subscriptions drive dynamic PR sync and notifications sync beyond what is listed in config.

### SSE event format

Each event is one JSON object on a `data:` line:

```
id: 42
data: {"id":42,"entity_type":"pull_request","entity_id":7,"event":"updated","repo_slug":"myorg/backend","payload":{...},"occurred_at":"2026-03-25T10:00:00Z"}

```

### Config keys

```toml
[global]
cmd_port              = 7748   # HTTP server port (default 7748)
heartbeat_ttl_seconds = 30     # subscription TTL in seconds (default 30)
```

## Database

SQLite, WAL mode. Key tables:

| Table | Contents |
|-------|----------|
| `repo` | Repos from config |
| `branch` | Branches with SHA, ahead/behind counts |
| `pull_request` | Open and recently-closed PRs |
| `pr_review` | PR review states per author |
| `notification` | GitHub notifications |
| `repo_event` | Raw GitHub Events API payloads |
| `checkout` | Last recorded checkout SHA per branch |
| `call_log` | Every API call with timing and rate limit data |
| `change_log` | Append-only log of every entity change (IPC bus) |
| `poll_state` | ETag / Last-Modified per endpoint for conditional requests |

Views: `v_open_prs`, `v_unread_notifications`, `v_recent_events`, `v_rate_limit`.

## Rate limiting

Two pools are tracked independently: REST and GraphQL cost points.
When remaining capacity drops below a threshold, syncs sleep until the reset window.
Progressive back-off increases sleep duration as capacity drops further.

## Architecture

- `gh` CLI is the transport (inherits your existing `gh auth` credentials, no separate token needed)
- ETag / Last-Modified conditional requests mean most polls are free 304s
- Events API is the cheapest signal: one paginated REST call reveals which PR numbers changed
- Those PR numbers are batched into a single aliased GraphQL query per repo (targeted sync)
- `change_log` is an append-only table; the HTTP `/events` SSE endpoint tails it and pushes rows to subscribers
- `ghcache-client` is a companion Rust library with typed queries and an `EventStream` for the SSE feed -- see [ghcache-client/README.md](ghcache-client/README.md)

## Appendix: Security & I/O Bill of Materials

This section documents every I/O boundary in ghcache: what crosses the process boundary, in which direction, with what cardinality, and what security properties constrain it.

### I/O Boundary Summary

| Boundary | Direction | Cardinality | Auth | Notes |
|----------|-----------|-------------|------|-------|
| `gh api` (REST) | Outbound | 1 call per endpoint per poll interval per repo | Delegated to `gh` CLI | ETag/Last-Modified; most polls are free 304s |
| `gh api graphql` | Outbound | 1 call per repo per sync pass (batched) | Delegated to `gh` CLI | Multiple PRs aliased into single query |
| `gh repo clone` | Outbound | 1 per repo (first checkout only) | Delegated to `gh` CLI | Subsequent updates use `git fetch` |
| `git fetch` / `git reset` | Outbound | 1 per branch per sync pass (SHA-gated) | System SSH/git config | Skipped if DB SHA matches checkout SHA |
| HTTP server | Inbound | N concurrent SSE clients + command POSTs | None (loopback-only) | Bound to `127.0.0.1`, not `0.0.0.0` |
| SQLite writes | Local | 1 upsert per entity per sync pass | Filesystem permissions | WAL mode, foreign keys enforced |
| `change_log` inserts | Local | 1 per entity mutation (append-only) | Filesystem permissions | Immutable audit trail, tailed by SSE |
| `call_log` inserts | Local | 1 per API call (append-only) | Filesystem permissions | Rate limit telemetry, never updated |
| Config file read | Local | 1 at startup | Filesystem permissions | Read-only after startup |
| Pidfile | Local | 1 write at startup, 1 delete on exit | Filesystem permissions | Prevents concurrent instances |
| stdout JSON lines | Local | 1 per API call (opt-in) | Process stdout | Rate limit summaries and per-call detail |

### External CLI Calls

All GitHub API access goes through the `gh` CLI binary (configurable via `gh_binary` in config).

| Command | Purpose | Source Location |
|---------|---------|-----------------|
| `gh api --include <endpoint>` | REST API calls (all endpoints) | `src/gh.rs:177` |
| `gh api graphql --include -f query=...` | GraphQL queries | `src/gh.rs:263` |
| `gh repo clone <slug> <dest> -- --branch <branch>` | Initial git checkout | `src/checkout.rs:173` |
| `gh repo clone <slug> <dest>` | Ensure repo for HTTP subscriber | `src/cmd.rs:383` |

**REST Endpoints Called:**

| Endpoint | Purpose | Source |
|----------|---------|--------|
| `/repos/{owner}/{name}/events` | Repo activity events | `src/sync/events.rs:16` |
| `/orgs/{owner}/events` | Org-wide activity events | `src/sync/events.rs:109` |
| `/repos/{owner}/{name}/branches` | Branch SHAs | `src/sync/branches.rs:15` |
| `/notifications` | User notifications | `src/sync/notifications.rs:8` |
| `/orgs/{owner}/repos` | Discover org repos | `src/sync/mod.rs:48` |
| `/users/{owner}/repos` | Discover user repos | `src/sync/mod.rs:49` |
| `graphql` | PR data (full + targeted) | `src/sync/prs.rs:74,130` |

### Git Commands

| Command | Purpose | Source Location |
|---------|---------|-----------------|
| `git -C <path> fetch origin <branch>` | Update existing checkout | `src/checkout.rs:188` |
| `git -C <path> reset --hard FETCH_HEAD` | Reset to fetched SHA | `src/checkout.rs:195` |
| `git -C <path> fetch --all` | Fetch all refs (HTTP subscribe) | `src/cmd.rs:388` |

### HTTP Server

Binds to `127.0.0.1:{cmd_port}` (default `7748`). Loopback only -- not exposed externally.

| Method | Endpoint | Body | Response | Source |
|--------|----------|------|----------|--------|
| `GET` | `/events` | -- | SSE stream of `change_log` rows | `src/cmd.rs:320` |
| `POST` | `/subscribe` | `{"uuid","owner","repo","pr_sync","notifications"}` | `{"path":"..."}` | `src/cmd.rs:333` |
| `POST` | `/heartbeat` | `{"uuid"}` | `{"ok":bool}` | `src/cmd.rs:340` |
| `POST` | `/pause` | -- | `{"ok":true}` | `src/cmd.rs:345` |
| `POST` | `/resume` | -- | `{"ok":true}` | `src/cmd.rs:350` |

### Filesystem Operations

| Path | Operation | Purpose | Source |
|------|-----------|---------|--------|
| `ghcache.toml` (various locations) | Read | Config loading | `src/config.rs:110` |
| `~/.config/ghcache/config.toml` | Write (via `init`) | Template creation | `src/main.rs:268` |
| `{db_path}` (default `~/.local/share/ghcache/gh.db`) | Read/Write | SQLite database | `src/db.rs` |
| `{staging_folder}/{owner}/{repo}/` | Read/Write | Git checkouts | `src/checkout.rs` |
| `{db_path}.pid` | Write/Delete | Pidfile for instance lock | `src/pidfile.rs` |

### SQLite Schema

| Table | Purpose | Write Pattern | Cardinality |
|-------|---------|---------------|-------------|
| `repo` | Configured repos | Upsert | 1 per configured repo |
| `branch` | Branch SHAs | Upsert | 1 per tracked branch per repo |
| `pull_request` | PR metadata | Upsert | 1 per open PR per repo per sync |
| `pr_review` | PR review states | Upsert | N per PR (one per reviewer) |
| `pr_label` | PR labels (junction) | Delete + Insert | N per PR (replaced in full each sync) |
| `pr_requested_reviewer` | Requested reviewers (junction) | Delete + Insert | N per PR (replaced in full each sync) |
| `pr_status_check` | CI status checks | Upsert | N per PR (one per context) |
| `notification` | GitHub notifications | Upsert | 1 per notification |
| `repo_event` | Raw event payloads | `INSERT OR IGNORE` | Idempotent; bounded by Events API page size |
| `checkout` | Last checkout SHA per branch | Upsert | 1 per checked-out branch |
| `call_log` | Every API call with rate limit data | Append-only | 1 row per API call, never deleted |
| `change_log` | Entity change events (IPC bus) | Append-only | 1 row per entity mutation, never deleted |
| `poll_state` | ETag/Last-Modified per endpoint | Upsert | 1 per polled endpoint |
| `subscription` | HTTP subscriber state | In-memory | Transient, not persisted |

### Data Leaving the Machine

| Data | Destination | Trigger | Cardinality |
|------|-------------|---------|-------------|
| GitHub API requests (repo/org/branch identifiers, pagination cursors, ETags) | `api.github.com` | Sync operations | Bounded by configured repos x poll interval |
| SSE events to local subscribers | `127.0.0.1:{cmd_port}` | `change_log` inserts | 1 broadcast per mutation per connected client |

### Data Stored Locally

| Data | Sensitivity | Location |
|------|-------------|----------|
| GitHub API responses (PR content, event payloads) | Medium | SQLite `*_json` columns |
| Rate limit state | Low | SQLite `call_log`, `poll_state` |
| Git repository contents | High (full source code) | `{staging_folder}` |
| Auth tokens | **None** | Delegated to `gh` CLI keychain |

### Security Properties

- **No tokens stored** -- auth delegated to `gh` CLI (uses OS keychain). ghcache never reads, logs, or caches credentials.
- **Loopback-only HTTP** -- command server binds `127.0.0.1`, not `0.0.0.0`. No authentication on the HTTP server; security relies on network isolation.
- **No outbound HTTP client** -- all network egress goes through `gh` subprocess invocation, not a direct HTTP client in the ghcache process.
- **Conditional requests** -- ETag/Last-Modified headers on every REST call minimize data transfer and API quota consumption.
- **Idempotent writes** -- `INSERT OR IGNORE` / `ON CONFLICT DO UPDATE` prevent duplication from retries or overlapping syncs.
- **Append-only audit logs** -- `call_log` and `change_log` are insert-only with auto-timestamps. No UPDATE or DELETE operations target these tables.
- **PID-based instance lock** -- pidfile with `libc::kill(pid, 0)` liveness check prevents concurrent watch daemons on the same database.
- **No user-controlled SQL** -- dynamic query construction in `query/prs.rs` uses manual escaping (`'` to `''`), not string interpolation of raw input.
