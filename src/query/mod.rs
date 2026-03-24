pub mod events;
pub mod notifications;
pub mod prs;
pub mod sql;

use anyhow::Result;
use clap::Subcommand;
use rusqlite::Connection;

use crate::output::Format;

#[derive(Subcommand)]
pub enum QueryCmd {
    /// List pull requests
    Prs {
        /// Filter by repo (owner/name)
        #[arg(long)]
        repo: Option<String>,
        /// PR state: open, closed, merged
        #[arg(long, default_value = "open")]
        state: String,
        /// Only PRs with no approvals
        #[arg(long)]
        needs_review: bool,
        /// Only PRs authored by the current gh user
        #[arg(long)]
        mine: bool,
        /// Output format
        #[arg(long, value_enum)]
        format: Option<Format>,
    },
    /// Show a single PR with reviews and comments
    Pr {
        number: u32,
        #[arg(long)]
        repo: String,
        #[arg(long, value_enum)]
        format: Option<Format>,
    },
    /// List unread notifications
    Notifications {
        /// Also mark as read via API
        #[arg(long)]
        mark_read: bool,
        #[arg(long, value_enum)]
        format: Option<Format>,
    },
    /// List recent repo events
    Events {
        #[arg(long)]
        repo: Option<String>,
        #[arg(long)]
        r#type: Option<String>,
        #[arg(long, value_enum)]
        format: Option<Format>,
    },
    /// Show tracked branches
    Branches {
        #[arg(long)]
        repo: Option<String>,
        #[arg(long, value_enum)]
        format: Option<Format>,
    },
    /// Recent API call log with rate limit info
    RateLimit {
        #[arg(long, value_enum)]
        format: Option<Format>,
    },
    /// Raw SQL passthrough
    Sql {
        query: String,
        #[arg(long, value_enum)]
        format: Option<Format>,
    },
}

pub fn run(conn: &Connection, cmd: QueryCmd) -> Result<()> {
    match cmd {
        QueryCmd::Prs { repo, state, needs_review, mine, format } => {
            prs::query(conn, repo.as_deref(), &state, needs_review, mine, format.unwrap_or_else(Format::auto))
        }
        QueryCmd::Pr { number, repo, format } => {
            prs::query_one(conn, number, &repo, format.unwrap_or_else(Format::auto))
        }
        QueryCmd::Notifications { mark_read, format } => {
            notifications::query(conn, mark_read, format.unwrap_or_else(Format::auto))
        }
        QueryCmd::Events { repo, r#type, format } => {
            events::query(conn, repo.as_deref(), r#type.as_deref(), format.unwrap_or_else(Format::auto))
        }
        QueryCmd::Branches { repo, format } => {
            query_branches(conn, repo.as_deref(), format.unwrap_or_else(Format::auto))
        }
        QueryCmd::RateLimit { format } => {
            sql::query_view(conn, "v_rate_limit", format.unwrap_or_else(Format::auto))
        }
        QueryCmd::Sql { query, format } => {
            sql::query_raw(conn, &query, format.unwrap_or_else(Format::auto))
        }
    }
}

fn query_branches(conn: &Connection, repo: Option<&str>, format: Format) -> Result<()> {
    let mut where_clause = String::new();
    if let Some(slug) = repo {
        let parts: Vec<&str> = slug.splitn(2, '/').collect();
        if parts.len() == 2 {
            where_clause = format!(
                " WHERE r.owner = '{}' AND r.name = '{}'",
                parts[0].replace('\'', "''"),
                parts[1].replace('\'', "''")
            );
        }
    }

    let query = format!(
        "SELECT r.owner || '/' || r.name AS repo_slug, b.name, b.sha, b.behind_default, b.ahead_default, b.updated_at
         FROM branch b
         JOIN repo r ON r.id = b.repo_id{where_clause}
         ORDER BY r.owner, r.name, b.name"
    );

    sql::query_raw(conn, &query, format)
}
