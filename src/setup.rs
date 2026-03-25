use anyhow::{bail, Context, Result};
use inquire::{Confirm, Select, Text, validator::Validation};
use std::path::PathBuf;
use std::process::Command;

fn split_csv(s: &str) -> Vec<String> {
    s.split(',')
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .collect()
}

fn to_toml_string_list(v: &[String]) -> String {
    let items: Vec<String> = v.iter().map(|s| format!("\"{}\"", s)).collect();
    format!("[{}]", items.join(", "))
}

pub fn run(config_path: Option<&std::path::Path>) -> Result<()> {
    let owner = Text::new("GitHub username or org")
        .with_help_message("Run: gh api user --jq '.login'")
        .with_placeholder("yourname")
        .with_validator(|s: &str| {
            if s.trim().is_empty() {
                Ok(Validation::Invalid("owner is required".into()))
            } else {
                Ok(Validation::Valid)
            }
        })
        .prompt()?;

    let scope_options = vec!["All repos in account", "Specific repos only"];
    let scope = Select::new("Repo scope", scope_options).prompt()?;
    let all_repos = scope == "All repos in account";

    let specific_repos: Vec<String> = if !all_repos {
        let raw = Text::new("Repo names")
            .with_help_message("Comma-separated: backend,frontend,infra")
            .with_placeholder("repo1,repo2")
            .prompt()?;
        split_csv(&raw)
    } else {
        vec![]
    };

    let exclude: Vec<String> = if all_repos {
        let raw = Text::new("Exclude repos (optional)")
            .with_help_message("Comma-separated repo names to skip")
            .with_placeholder("scratch,archived-thing")
            .with_default("")
            .prompt()?;
        split_csv(&raw)
    } else {
        vec![]
    };

    let branches_raw = Text::new("Branches to track")
        .with_help_message("Comma-separated, glob * supported. e.g: main,staging,release/*")
        .with_default("main")
        .prompt()?;
    let sync_branches = split_csv(&branches_raw);

    let sync_prs = Confirm::new("Sync pull requests?")
        .with_default(true)
        .prompt()?;

    let sync_events = Confirm::new("Sync repo events? (drives targeted PR sync)")
        .with_default(true)
        .prompt()?;

    let db_path = Text::new("Database path")
        .with_default("~/.local/share/ghcache/gh.db")
        .prompt()?;

    let staging_raw = Text::new("Staging folder for git checkouts (optional)")
        .with_help_message("Leave empty to skip checkout support")
        .with_placeholder("~/src/staging")
        .with_default("~/.local/share/ghcache/repos")
        .prompt()?;
    let staging_folder = staging_raw.trim().to_string();

    let toml = generate_toml(
        owner.trim(),
        all_repos,
        &specific_repos,
        &exclude,
        sync_prs,
        sync_events,
        &sync_branches,
        db_path.trim(),
        &staging_folder,
    );

    let dest = config_path
        .map(|p| p.to_owned())
        .unwrap_or_else(|| PathBuf::from("ghcache.toml"));

    if dest.exists() {
        bail!("{} already exists -- delete it first or pass --config to write elsewhere", dest.display());
    }

    std::fs::write(&dest, &toml)?;
    eprintln!("\nWrote {}\n", dest.display());
    eprint!("{}", toml);

    eprintln!("\nRunning initial sync...\n");
    let status = Command::new(std::env::current_exe()?)
        .args(["--config", &dest.to_string_lossy(), "sync"])
        .status()
        .context("running ghcache sync")?;

    if !status.success() {
        bail!("sync exited with {status}");
    }

    eprintln!("\nDone. Start the watch loop with:\n  ghcache --config {} watch\n", dest.display());
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn generate_toml(
    owner: &str,
    all_repos: bool,
    repos: &[String],
    exclude: &[String],
    sync_prs: bool,
    sync_events: bool,
    sync_branches: &[String],
    db_path: &str,
    staging_folder: &str,
) -> String {
    let mut out = String::new();

    out.push_str("[global]\n");
    out.push_str(&format!("db_path               = \"{db_path}\"\n"));
    if !staging_folder.is_empty() {
        out.push_str(&format!("staging_folder        = \"{staging_folder}\"\n"));
    }
    out.push_str("poll_interval_seconds = 60\n");
    out.push_str("log_level             = \"info\"\n");
    out.push_str("gh_binary             = \"gh\"\n");

    if all_repos {
        out.push_str("\n[[org]]\n");
        out.push_str(&format!("owner         = \"{owner}\"\n"));
        out.push_str(&format!("sync_prs      = {sync_prs}\n"));
        out.push_str(&format!("sync_events   = {sync_events}\n"));
        out.push_str(&format!("sync_branches = {}\n", to_toml_string_list(sync_branches)));
        if !exclude.is_empty() {
            out.push_str(&format!("exclude       = {}\n", to_toml_string_list(exclude)));
        }
    } else {
        for name in repos {
            out.push_str("\n[[repo]]\n");
            out.push_str(&format!("owner         = \"{owner}\"\n"));
            out.push_str(&format!("name          = \"{name}\"\n"));
            out.push_str("default_branch = \"main\"\n");
            out.push_str(&format!("sync_prs      = {sync_prs}\n"));
            out.push_str(&format!("sync_events   = {sync_events}\n"));
            out.push_str(&format!("sync_branches = {}\n", to_toml_string_list(sync_branches)));
        }
    }

    out
}
