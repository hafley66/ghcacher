mod checkout;
mod cmd;
mod config;
mod db;
mod gh;
mod output;
mod pidfile;
mod query;
mod setup;
mod sync;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use sqlx::SqlitePool;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

#[derive(Parser)]
#[command(
    name = "ghcache",
    about = "GitHub CLI cache layer backed by SQLite",
    long_about = "GitHub CLI cache layer backed by SQLite.\n\n\
        Config file search order:\n  \
          1. --config <path>\n  \
          2. $GHCACHE_CONFIG\n  \
          3. ./ghcache.toml\n  \
          4. ~/.config/ghcache/config.toml\n\n\
        Global flags:\n  \
          --silent / -s      suppress JSON rate-limit summary lines on stdout\n  \
          --calls-to-stdout  print each API call as a JSON line (endpoint, cost, duration)\n\n\
        Rate-limit summary (emitted after every call, suppressed by --silent):\n  \
          {\"ts\":\"...\",\"gh_points_rest_remaining\":4800,\"gh_points_graphql_remaining\":4950}\n\
        Per-call detail (emitted when --calls-to-stdout is set):\n  \
          {\"ts\":\"...\",\"api_type\":\"graphql\",\"endpoint\":\"...\",\"gql_cost\":1,\"rate_remaining\":4950,\"duration_ms\":150}\n\n\
        Typical workflow:\n  \
          ghcache setup       # guided TUI setup (recommended)\n  \
          ghcache init        # write a config template manually\n  \
          ghcache sync        # full initial sync\n  \
          ghcache watch       # continuous polling loop + HTTP server on 127.0.0.1:7748\n\n\
        Run `ghcache readme` for full documentation."
)]
struct Cli {
    /// Path to config file (overrides $GHCACHE_CONFIG and default locations)
    #[arg(long, global = true)]
    config: Option<PathBuf>,

    /// Suppress JSON rate-limit log lines on stdout
    #[arg(short = 's', long, global = true)]
    silent: bool,

    /// Print each API call as a JSON line on stdout (endpoint, status, rate_remaining, gql_cost, duration_ms)
    #[arg(long, global = true)]
    calls_to_stdout: bool,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Interactive TUI setup -- guided config creation + initial sync
    #[command(long_about = "Interactive terminal prompts to collect your GitHub account details, \
        write ghcache.toml, and run the initial sync automatically.\n\n\
        To write the config to a custom path:\n  \
          ghcache --config ~/.config/ghcache/config.toml setup\n\n\
        The optional ghcache-init companion binary (Go + Charm huh) provides a richer form UI \
        as an alternative. Build it: cd init-form && go build -o ghcache-init .")]
    Setup,
    /// Write a config template to ghcache.toml (non-interactive)
    Init,
    /// Print the resolved config (for debugging)
    Config,
    /// Print the resolved database path -- useful for: sqlite3 $(ghcache db-path)
    DbPath,
    /// Show sync state: last poll times, rate limit, DB size
    Status,
    /// Sync repos per config (full sweep)
    #[command(long_about = "Full sweep sync: fetches all open PRs, events, notifications, and \
        branches for every configured repo.\n\n\
        --repo restricts to one repo; --prs/--notifications/--events restrict to one data type.\n\
        Always performs a full GraphQL PR fetch regardless of event hints.")]
    Sync {
        /// Restrict to one repo (owner/name)
        #[arg(long)]
        repo: Option<String>,
        /// Sync only PRs
        #[arg(long)]
        prs: bool,
        /// Sync only notifications
        #[arg(long)]
        notifications: bool,
        /// Sync only events
        #[arg(long)]
        events: bool,
    },
    /// Continuous polling loop (event-targeted after first pass)
    #[command(long_about = "Continuous polling loop.\n\n\
        On startup, skips the full sweep if the DB already has PR data (cached from a \
        previous run). Use --full-sweep to force a complete re-fetch.\n\n\
        Each iteration is event-targeted: only PRs mentioned in GitHub event payloads \
        are re-fetched via GraphQL. Full-sweep PR queries are batched (up to 20 repos \
        per GraphQL call) to minimize API consumption. Most polls are free 304 responses.\n\n\
        Repos with checkout_on_sync = true in config have their sync_branches checked out \
        into staging_folder after each pass.\n\n\
        Also starts the HTTP command server on 127.0.0.1:{cmd_port} (default 7748).\n\
        Endpoints: GET /events (SSE), POST /subscribe, POST /heartbeat, POST /pause, POST /resume.\n\n\
        --full-sweep: force a full PR re-fetch on startup even if data is cached.\n\
        --no-sync: start the HTTP server only; skip the sync loop entirely.\n\
        --daemon: double-fork to background, redirect stdio to /dev/null. \
        A pidfile is written next to the database (gh.pid) and checked on startup \
        to prevent duplicate instances.")]
    Watch {
        /// Fork to background; write pidfile next to the database
        #[arg(long)]
        daemon: bool,
        /// Start HTTP server only; skip sync loop
        #[arg(long)]
        no_sync: bool,
        /// Force a full sweep on startup even if data is cached
        #[arg(long)]
        full_sweep: bool,
    },
    /// Query cached data
    Query {
        #[command(subcommand)]
        sub: query::QueryCmd,
    },
    /// Clone or update a branch into staging_folder
    #[command(long_about = "Clone or update a branch into staging_folder.\n\n\
        Path convention: {staging_folder}/{owner}/{name}\n\
        Example: myorg/backend branch main  ->  ~/src/staging/myorg/backend\n\n\
        First checkout: gh repo clone owner/name dest -- --branch branch\n\
        Subsequent:     git fetch origin branch && git reset --hard FETCH_HEAD\n\
        (skipped if branch.sha in DB matches last recorded checkout sha)\n\n\
        Requires staging_folder in config and the repo to exist in the DB (run sync first).\n\
        watch runs this automatically for repos with checkout_on_sync = true.")]
    Checkout {
        /// owner/name
        repo: String,
        branch: String,
    },
    /// Print full documentation (README)
    Readme,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let cfg = match &cli.cmd {
        Cmd::Init => {
            return cmd_init();
        }
        Cmd::Setup => {
            return setup::run(cli.config.as_deref());
        }
        Cmd::Readme => {
            print!("{}", include_str!("../README.md"));
            return Ok(());
        }
        _ => config::load(cli.config.as_deref())?,
    };

    // Daemonize before tokio runtime so we don't fork after threading starts.
    if matches!(cli.cmd, Cmd::Watch { daemon: true, .. }) {
        #[cfg(unix)]
        pidfile::daemonize()?;
        #[cfg(not(unix))]
        anyhow::bail!("--daemon is not supported on this platform");
    }

    let level = cfg.log_level.parse().unwrap_or(tracing::Level::INFO);
    tracing_subscriber::fmt()
        .with_max_level(level)
        .with_target(false)
        .init();

    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async_main(cli, cfg))
}

async fn async_main(cli: Cli, cfg: config::ResolvedConfig) -> Result<()> {
    match cli.cmd {
        Cmd::Init | Cmd::Setup | Cmd::Readme => unreachable!(),
        Cmd::Config => cmd_config(&cfg),
        Cmd::DbPath => {
            println!("{}", cfg.db_path.display());
            Ok(())
        }
        Cmd::Status => {
            let pool = db::open(&cfg.db_path).await?;
            cmd_status(&pool, &cfg).await
        }
        Cmd::Sync { repo, prs, notifications, events } => {
            let pool = db::open(&cfg.db_path).await?;
            let gh = gh::GhClient::new(&cfg.gh_binary, cfg.rate_warn_threshold, cfg.rate_stop_threshold, cli.silent, cli.calls_to_stdout);
            let username = gh::authenticated_username(&cfg.gh_binary).await?;
            let filter = sync::SyncFilter { repo, prs_only: prs, notifs_only: notifications, events_only: events };
            sync::run(&pool, &gh, &cfg, filter, true, &[], &[], &username).await
        }
        Cmd::Watch { daemon: _, no_sync, full_sweep } => {
            let pid_path = cfg.db_path.with_extension("pid");
            let _pid = pidfile::PidFile::acquire(&pid_path)?;
            let pool = db::open(&cfg.db_path).await?;
            let gh = gh::GhClient::new(&cfg.gh_binary, cfg.rate_warn_threshold, cfg.rate_stop_threshold, cli.silent, cli.calls_to_stdout);
            let paused = Arc::new(AtomicBool::new(false));
            let subs = {
                let subs = cmd::Subscriptions::new(std::time::Duration::from_secs(cfg.heartbeat_ttl_seconds));
                let subs_server = Arc::clone(&subs);
                let staging = cfg.staging_folder.clone();
                let pool_cmd = pool.clone();
                let port = cfg.cmd_port;
                let paused_server = Arc::clone(&paused);
                let aliases = Arc::new(cfg.owner_fs_aliases.clone());
                tokio::spawn(async move {
                    if let Err(e) = cmd::run(subs_server, staging, pool_cmd, port, paused_server, aliases).await {
                        tracing::error!(error = %e, "cmd server exited");
                    }
                });
                Some(subs)
            };
            if no_sync {
                tracing::info!("--no-sync: HTTP server running, sync loop disabled");
                loop { tokio::time::sleep(std::time::Duration::from_secs(3600)).await; }
            }
            sync::watch(&pool, &gh, &cfg, subs, paused, full_sweep).await
        }
        Cmd::Query { sub } => {
            let pool = db::open(&cfg.db_path).await?;
            query::run(&pool, sub).await
        }
        Cmd::Checkout { repo, branch } => {
            let pool = db::open(&cfg.db_path).await?;
            let (owner, name) = checkout::parse_slug(&repo)?;
            let fs_owner = cfg.owner_fs_aliases.get(&owner).map(|s| s.as_str());
            checkout::checkout_one(&pool, &cfg.staging_folder, &owner, &name, &branch, fs_owner).await
        }
    }
}

fn cmd_init() -> Result<()> {
    let template = r#"[global]
db_path               = "~/.local/share/ghcache/gh.db"
staging_folder        = "~/.local/share/ghcache/repos"
poll_interval_seconds = 60
log_level             = "info"
gh_binary             = "gh"

# Sync all repos under a GitHub user or org:
[[org]]
owner         = "myorg"
sync_prs      = true
sync_events   = true
sync_branches = ["main"]
exclude       = []

# Or target specific repos:
# [[repo]]
# owner          = "myorg"
# name           = "backend"
# default_branch = "main"
# sync_prs       = true
# sync_events    = true
# sync_branches  = ["main", "staging"]
"#;
    let path = dirs::config_dir()
        .map(|d| d.join("ghcache").join("config.toml"))
        .unwrap_or_else(|| PathBuf::from("ghcache.toml"));

    if path.exists() {
        anyhow::bail!("{} already exists", path.display());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(&path, template)?;
    println!("Created {}", path.display());
    println!("Edit the [[org]] / [[repo]] entries then run: ghcache sync");
    Ok(())
}

fn cmd_config(cfg: &config::ResolvedConfig) -> Result<()> {
    println!("db_path:          {}", cfg.db_path.display());
    println!("staging_folder:   {}", cfg.staging_folder.display());
    println!("poll_interval:    {}s", cfg.poll_interval_seconds);
    println!("log_level:        {}", cfg.log_level);
    println!("gh_binary:        {}", cfg.gh_binary);
    println!("repos ({}):", cfg.repos.len());
    for r in &cfg.repos {
        println!("  {}/{}", r.owner, r.name);
    }
    Ok(())
}

async fn cmd_status(pool: &SqlitePool, cfg: &config::ResolvedConfig) -> Result<()> {
    let db_size: i64 = sqlx::query_scalar(
        "SELECT page_count * page_size FROM pragma_page_count(), pragma_page_size()",
    )
    .fetch_one(pool)
    .await
    .unwrap_or(0);

    let pr_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pull_request")
        .fetch_one(pool)
        .await
        .unwrap_or(0);
    let notif_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM notification WHERE unread=1")
        .fetch_one(pool)
        .await
        .unwrap_or(0);

    println!("db:               {} ({:.1} KB)", cfg.db_path.display(), db_size as f64 / 1024.0);
    println!("pull_requests:    {}", pr_count);
    println!("unread notifs:    {}", notif_count);

    let rows: Vec<(String, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT endpoint, last_polled_at, last_changed_at FROM poll_state ORDER BY last_polled_at DESC LIMIT 10",
    )
    .fetch_all(pool)
    .await?;

    if !rows.is_empty() {
        println!("\nrecent polls:");
        for (ep, polled, changed) in rows {
            println!(
                "  {} -- polled: {} changed: {}",
                ep,
                polled.as_deref().unwrap_or("never"),
                changed.as_deref().unwrap_or("never"),
            );
        }
    }

    Ok(())
}
