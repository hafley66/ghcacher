mod checkout;
mod config;
mod db;
mod gh;
mod output;
mod query;
mod serve;
mod setup;
mod sync;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

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
        Typical workflow:\n  \
          ghcache setup       # guided TUI setup (recommended)\n  \
          ghcache init        # write a config template manually\n  \
          ghcache sync        # full initial sync\n  \
          ghcache watch       # continuous polling loop\n\n\
        Run `ghcache readme` for full documentation."
)]
struct Cli {
    /// Path to config file (overrides $GHCACHE_CONFIG and default locations)
    #[arg(long, global = true)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Interactive TUI setup -- guided config creation + initial sync
    #[command(long_about = "Launches a terminal form (requires ghcache-init on PATH or next to \
        the ghcache binary) to collect your GitHub account details, writes ghcache.toml, \
        and runs the initial sync automatically.\n\n\
        Build the form: cd init-form && go build -o ghcache-init .")]
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
        First iteration: full sweep (same as `ghcache sync`).\n\
        Subsequent iterations: event-targeted -- only PRs mentioned in GitHub event payloads \
        are re-fetched via GraphQL. Most polls are free 304 Not Modified responses.\n\n\
        Repos with checkout_on_sync = true in config have their sync_branches checked out \
        into staging_folder after each pass.\n\n\
        --daemon is not yet implemented.")]
    Watch {
        /// Fork to background (not yet implemented)
        #[arg(long)]
        daemon: bool,
    },
    /// Query cached data
    Query {
        #[command(subcommand)]
        sub: query::QueryCmd,
    },
    /// Broadcast change_log rows as NDJSON over a Unix socket
    #[command(long_about = "Tails change_log and broadcasts new rows as NDJSON to all connected \
        Unix socket subscribers.\n\n\
        Default socket: $XDG_RUNTIME_DIR/ghcache.sock or /tmp/ghcache.sock.\n\
        Use ghcache-client for a typed Rust subscriber.")]
    Serve {
        /// Unix socket path (default: $XDG_RUNTIME_DIR/ghcache.sock or /tmp/ghcache.sock)
        #[arg(long)]
        socket: Option<std::path::PathBuf>,
    },
    /// Clone or update a branch into staging_folder
    #[command(long_about = "Clone or update a branch into staging_folder.\n\n\
        Path convention: {staging_folder}/{owner}/{branch}/{name}\n\
        Example: myorg/backend branch main  ->  ~/src/staging/myorg/main/backend\n\n\
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

    let level = cfg.log_level.parse().unwrap_or(tracing::Level::INFO);
    tracing_subscriber::fmt()
        .with_max_level(level)
        .with_target(false)
        .init();

    match cli.cmd {
        Cmd::Init | Cmd::Setup | Cmd::Readme => unreachable!(),
        Cmd::Config => cmd_config(&cfg),
        Cmd::DbPath => {
            println!("{}", cfg.db_path.display());
            Ok(())
        }
        Cmd::Status => {
            let conn = db::open(&cfg.db_path)?;
            cmd_status(&conn, &cfg)
        }
        Cmd::Sync { repo, prs, notifications, events } => {
            let conn = db::open(&cfg.db_path)?;
            let gh = gh::GhClient::new(&cfg.gh_binary, cfg.rate_warn_threshold, cfg.rate_stop_threshold);
            let filter = sync::SyncFilter { repo, prs_only: prs, notifs_only: notifications, events_only: events };
            sync::run(&conn, &gh, &cfg, filter, true)
        }
        Cmd::Watch { daemon } => {
            if daemon {
                anyhow::bail!("--daemon not yet implemented");
            }
            let conn = db::open(&cfg.db_path)?;
            let gh = gh::GhClient::new(&cfg.gh_binary, cfg.rate_warn_threshold, cfg.rate_stop_threshold);
            sync::watch(&conn, &gh, &cfg)
        }
        Cmd::Query { sub } => {
            let conn = db::open(&cfg.db_path)?;
            query::run(&conn, sub)
        }
        Cmd::Serve { socket } => {
            let conn = db::open(&cfg.db_path)?;
            let path = socket.unwrap_or_else(serve::default_socket_path);
            serve::run(&conn, &path)
        }
        Cmd::Checkout { repo, branch } => {
            let staging = cfg.staging_folder.as_deref()
                .ok_or_else(|| anyhow::anyhow!("staging_folder not set in config"))?;
            let conn = db::open(&cfg.db_path)?;
            let (owner, name) = checkout::parse_slug(&repo)?;
            checkout::checkout_one(&conn, staging, &owner, &name, &branch)
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
    let path = PathBuf::from("ghcache.toml");
    if path.exists() {
        anyhow::bail!("ghcache.toml already exists");
    }
    std::fs::write(&path, template)?;
    println!("Created ghcache.toml -- edit repos then run: ghcache sync");
    Ok(())
}

fn cmd_config(cfg: &config::ResolvedConfig) -> Result<()> {
    println!("db_path:          {}", cfg.db_path.display());
    println!(
        "staging_folder:   {}",
        cfg.staging_folder
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "(not set)".into())
    );
    println!("poll_interval:    {}s", cfg.poll_interval_seconds);
    println!("log_level:        {}", cfg.log_level);
    println!("gh_binary:        {}", cfg.gh_binary);
    println!("repos ({}):", cfg.repos.len());
    for r in &cfg.repos {
        println!("  {}/{}", r.owner, r.name);
    }
    Ok(())
}

fn cmd_status(conn: &rusqlite::Connection, cfg: &config::ResolvedConfig) -> Result<()> {
    let db_size: i64 = conn.query_row(
        "SELECT page_count * page_size FROM pragma_page_count(), pragma_page_size()",
        [],
        |r| r.get(0),
    ).unwrap_or(0);

    let pr_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM pull_request", [], |r| r.get(0))
        .unwrap_or(0);
    let notif_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM notification WHERE unread=1", [], |r| r.get(0))
        .unwrap_or(0);

    println!("db:               {} ({:.1} KB)", cfg.db_path.display(), db_size as f64 / 1024.0);
    println!("pull_requests:    {}", pr_count);
    println!("unread notifs:    {}", notif_count);

    let mut stmt = conn.prepare(
        "SELECT endpoint, last_polled_at, last_changed_at FROM poll_state ORDER BY last_polled_at DESC LIMIT 10",
    )?;
    let rows: Vec<(String, Option<String>, Option<String>)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .filter_map(|r| r.ok())
        .collect();

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
