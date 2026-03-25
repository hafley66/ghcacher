# ghcache

A local caching proxy between the `gh` CLI and a normalized SQLite database.
GitHub data is synced into SQLite; local tools query SQLite directly -- no live API calls at read time.

## Quick start

```sh
# Option A: guided TUI (requires ghcache-init -- see Setup below)
ghcache setup

# Option B: manual
ghcache init          # write ghcache.toml template
$EDITOR ghcache.toml  # edit owner/repos
ghcache sync          # full initial sync
ghcache watch         # continuous polling loop
```

## Setup (guided TUI)

`ghcache setup` runs an interactive terminal form and writes `ghcache.toml` for you.
It requires the `ghcache-init` companion binary (Go + Charm huh):

```sh
cd init-form
go build -o ghcache-init .
# Put it next to the ghcache binary, or on PATH:
cp ghcache-init /usr/local/bin/
```

Then:
```sh
ghcache setup          # launches the form, writes ghcache.toml, runs initial sync
```

To write the config somewhere other than `./ghcache.toml`:
```sh
ghcache --config ~/.config/ghcache/config.toml setup
```

## Demo (copy-paste)

Replace `yourname` with your GitHub login (`gh api user --jq '.login'`).

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
{"gh_points_graphql_remaining":4950,"gh_points_rest_remaining":4800,"ts":"2026-03-25T10:00:00Z"}
```

Both REST and GraphQL values are always shown together (latest recorded value for each pool).

### Per-call detail (`--calls-to-stdout` enables)

Each individual API call emits its own line with the endpoint and cost:

REST:
```json
{"api_type":"rest","duration_ms":42,"endpoint":"/repos/myorg/backend/events","rate_remaining":4800,"status":200,"ts":"2026-03-25T10:00:00Z"}
```

GraphQL:
```json
{"api_type":"graphql","duration_ms":150,"endpoint":"graphql:prs:full/myorg/backend","gql_cost":1,"rate_remaining":4950,"status":200,"ts":"2026-03-25T10:00:00Z"}
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

Also starts the HTTP command server (see below). Pass `--no-sync` to run the HTTP server only,
with no sync loop.

`--daemon` is not yet implemented.

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
- `ghcache-client` is a companion Rust library with typed queries and an `EventStream` for the SSE feed
