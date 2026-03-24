mod config;
mod db;
mod gh;
mod output;
mod query;
mod serve;
mod sync;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "ghcache", about = "GitHub CLI cache layer backed by SQLite")]
struct Cli {
    /// Path to config file (overrides $GHCACHE_CONFIG and default locations)
    #[arg(long, global = true)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create a config template and initialize the database
    Init,
    /// Print the resolved config (for debugging)
    Config,
    /// Print the resolved database path
    DbPath,
    /// Show sync state: last poll times, rate limit, DB size
    Status,
    /// Sync repos per config
    Sync {
        /// Sync a single repo (owner/name)
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
    /// Continuous sync loop
    Watch {
        /// Fork to background
        #[arg(long)]
        daemon: bool,
    },
    /// Query cached data
    Query {
        #[command(subcommand)]
        sub: query::QueryCmd,
    },
    /// Broadcast change_log events to Unix socket subscribers
    Serve {
        /// Unix socket path (default: $XDG_RUNTIME_DIR/ghcache.sock or /tmp/ghcache.sock)
        #[arg(long)]
        socket: Option<std::path::PathBuf>,
    },
    /// Checkout a branch into staging_folder
    Checkout {
        /// owner/name format
        repo: String,
        branch: String,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let cfg = match &cli.cmd {
        Cmd::Init => {
            return cmd_init();
        }
        _ => config::load(cli.config.as_deref())?,
    };

    let level = cfg.log_level.parse().unwrap_or(tracing::Level::INFO);
    tracing_subscriber::fmt()
        .with_max_level(level)
        .with_target(false)
        .init();

    match cli.cmd {
        Cmd::Init => unreachable!(),
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
            let gh = gh::GhClient::new(&cfg.gh_binary);
            let filter = sync::SyncFilter { repo, prs_only: prs, notifs_only: notifications, events_only: events };
            sync::run(&conn, &gh, &cfg, filter)
        }
        Cmd::Watch { daemon } => {
            if daemon {
                anyhow::bail!("--daemon not yet implemented");
            }
            let conn = db::open(&cfg.db_path)?;
            let gh = gh::GhClient::new(&cfg.gh_binary);
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
            anyhow::bail!("checkout not yet implemented: {repo}/{branch}");
        }
    }
}

fn cmd_init() -> Result<()> {
    let template = r#"[global]
db_path = "~/.local/share/ghcache/gh.db"
staging_folder = "~/src/staging"
poll_interval_seconds = 60
log_level = "info"
gh_binary = "gh"

[[repo]]
owner = "myorg"
name = "backend"
default_branch = "main"
sync_prs = true
sync_notifications = true
sync_events = true
sync_branches = ["main", "staging"]
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
