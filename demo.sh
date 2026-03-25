#!/usr/bin/env bash
# ghcache demo -- hafley66 repos, isolated to /tmp/ghcache-demo-$$
set -euo pipefail

DEMO_DIR="/tmp/ghcache-demo-$$"
BINARY="$(cd "$(dirname "$0")" && cargo build -q 2>/dev/null; echo "$(dirname "$0")/target/debug/ghcache")"
DB="$DEMO_DIR/gh.db"
CONFIG="$DEMO_DIR/config.toml"

cleanup() { rm -rf "$DEMO_DIR"; }
trap cleanup EXIT

mkdir -p "$DEMO_DIR"

# ── 1. Write config ─────────────────────────────────────────────────────────
cat > "$CONFIG" <<'TOML'
[global]
db_path              = "/tmp/ghcache-demo-REPLACE/gh.db"
poll_interval_seconds = 60
log_level            = "warn"
gh_binary            = "gh"

[[org]]
owner         = "hafley66"
sync_prs      = true
sync_events   = true
sync_branches = ["main"]
TOML

# Patch the db_path to include the actual PID
sed -i '' "s|/tmp/ghcache-demo-REPLACE|$DEMO_DIR|" "$CONFIG"

hr() { printf '\n\033[1;34m══ %s\033[0m\n' "$*"; }
run() { "$BINARY" --config "$CONFIG" "$@"; }
sq() { sqlite3 "$DB" "$@"; }

# ── 2. Show resolved config ──────────────────────────────────────────────────
hr "Resolved config"
run config

# ── 3. First sync (full sweep) ───────────────────────────────────────────────
hr "First sync -- full sweep, fetching all data"
time run sync

# ── 4. Status ───────────────────────────────────────────────────────────────
hr "Status after first sync"
run status

# ── 5. Show what landed in SQLite ───────────────────────────────────────────
hr "Repos and branch SHAs in DB"
sq -column -header "
SELECT r.owner || '/' || r.name AS repo, b.name AS branch, substr(b.sha,1,8) AS sha
FROM branch b JOIN repo r ON r.id = b.repo_id
ORDER BY repo, branch;"

hr "Open PRs (if any)"
sq -column -header "SELECT repo_slug, number, author, title FROM v_open_prs ORDER BY repo_slug, number;"

hr "Recent events"
sq -column -header "SELECT repo_slug, type, actor, subject, created_at FROM v_recent_events LIMIT 15;"

# ── 6. Second sync (should be mostly 304s) ──────────────────────────────────
hr "Second sync -- should be nearly instant (304 Not Modified)"
time run sync

# ── 7. Prove it was cheap ────────────────────────────────────────────────────
hr "Call log -- see the 304 cache hits"
sq -column -header "
SELECT endpoint, status_code, cache_hit, rate_remaining, duration_ms, called_at
FROM call_log
ORDER BY id DESC
LIMIT 20;"

hr "Rate limit pools"
sq -column -header "
SELECT
  CASE WHEN endpoint LIKE 'graphql%' THEN 'graphql' ELSE 'rest' END AS pool,
  rate_remaining,
  rate_resets_at,
  COUNT(*) AS calls
FROM v_rate_limit
GROUP BY pool;"

hr "DB size"
sq "SELECT printf('%.1f KB', page_count * page_size / 1024.0) AS db_size
    FROM pragma_page_count(), pragma_page_size();"

hr "Done -- DB at $DB (available until script exits)"
echo "  sqlite3 $DB"
echo "  sqlite3 $DB '.tables'"
echo "  sqlite3 $DB -column -header 'SELECT * FROM v_open_prs'"
echo ""
read -rp "Press enter to clean up and exit..."
