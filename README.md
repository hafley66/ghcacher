# ghcache

A local caching proxy between the `gh` CLI and a normalized SQLite database.
GitHub data is synced into SQLite; local tools query SQLite directly -- no live API calls at read time.

## Quick start

```sh
ghcache init          # write ghcache.toml template
$EDITOR ghcache.toml  # add your repos
ghcache sync          # full initial sync
ghcache watch         # continuous polling loop
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
staging_folder       = "~/src/staging"   # required for checkout
poll_interval_seconds = 60
log_level            = "info"            # trace | debug | info | warn | error
gh_binary            = "gh"

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
