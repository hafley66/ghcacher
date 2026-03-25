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

## Commands

### `ghcache init`
Writes a `ghcache.toml` template to the current directory and prints next steps.

### `ghcache sync [--repo owner/name] [--prs] [--notifications] [--events]`
Full sweep sync: fetches all open PRs, events, notifications, and branches for every configured repo.

- `--repo owner/name` restricts to one repo
- `--prs / --notifications / --events` restrict to one data type
- Always performs a full GraphQL PR fetch (not event-targeted)

### `ghcache watch [--daemon]`
Continuous polling loop. First iteration is a full sweep; subsequent iterations are event-targeted
(only PRs mentioned in PullRequestEvent / PullRequestReviewEvent payloads are re-fetched via GraphQL).

Runs `checkout` for repos with `checkout_on_sync = true` after each sync pass.

`--daemon` is not yet implemented.

### `ghcache checkout <owner/name> <branch>`
Clone or update a single branch into `staging_folder`.

Checkout path convention: `{staging_folder}/{owner}/{branch}/{name}`

Examples:
- `myorg/backend` branch `main`  → `~/src/staging/myorg/main/backend`
- `myorg/backend` branch `staging` → `~/src/staging/myorg/staging/backend`

First checkout: `gh repo clone owner/name dest -- --branch branch` (full history).
Subsequent checkouts: `git fetch origin branch && git reset --hard FETCH_HEAD`
(only runs if `branch.sha` in the DB differs from the last recorded checkout sha).

Requires `staging_folder` to be set in config. Requires the repo to exist in the DB
(run `ghcache sync` first).

### `ghcache query <subcommand>`
Query cached data from SQLite without hitting the GitHub API.

### `ghcache serve [--socket <path>]`
Broadcasts `change_log` rows as NDJSON over a Unix socket.
Default socket path: `$XDG_RUNTIME_DIR/ghcache.sock` or `/tmp/ghcache.sock`.

### `ghcache status`
Shows DB size, PR count, unread notification count, and recent poll state per endpoint.

### `ghcache config`
Prints the resolved config values (after tilde expansion, env var overrides, etc.).

### `ghcache db-path`
Prints the resolved database path. Useful for `sqlite3 $(ghcache db-path)`.

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
- `change_log` is an append-only table; `ghcache serve` tails it and broadcasts to Unix socket subscribers
- `ghcache-client` is a companion Rust library with typed queries and a `Subscriber` for the socket
