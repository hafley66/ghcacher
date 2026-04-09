use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub global: Global,
    #[serde(rename = "repo", default)]
    pub repos: Vec<RepoConfig>,
    #[serde(rename = "org", default)]
    pub orgs: Vec<OrgConfig>,
}

#[derive(Debug, Deserialize)]
pub struct Global {
    pub db_path: Option<String>,
    pub staging_folder: Option<String>,
    pub poll_interval_seconds: Option<u64>,
    pub log_level: Option<String>,
    pub gh_binary: Option<String>,
    /// REST + GraphQL remaining-calls count below which polling slows down (default 500).
    pub rate_warn_threshold: Option<i64>,
    /// REST + GraphQL remaining-calls count below which polling stops until reset (default 50).
    pub rate_stop_threshold: Option<i64>,
    /// Port for the cmd HTTP server on 127.0.0.1 (default 7748).
    pub cmd_port: Option<u16>,
    /// Seconds before a subscriber UUID is considered expired (default 30).
    pub heartbeat_ttl_seconds: Option<u64>,
    /// Sync the authenticated user's personal notification inbox (default false).
    pub sync_notifications: Option<bool>,
}

impl Default for Global {
    fn default() -> Self {
        Global {
            db_path: None,
            staging_folder: None,
            poll_interval_seconds: Some(60),
            log_level: Some("info".into()),
            gh_binary: Some("gh".into()),
            rate_warn_threshold: Some(500),
            rate_stop_threshold: Some(50),
            cmd_port: Some(7748),
            heartbeat_ttl_seconds: Some(30),
            sync_notifications: None,
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct RepoConfig {
    pub owner: String,
    pub name: String,
    pub default_branch: Option<String>,
    pub sync_prs: Option<bool>,
    pub sync_notifications: Option<bool>,
    pub sync_events: Option<bool>,
    pub sync_branches: Option<Vec<String>>,
    pub checkout_on_sync: Option<bool>,
    pub poll_interval_seconds: Option<u64>,
    /// Short alias used as the owner directory name on disk instead of `owner`.
    pub fs_alias: Option<String>,
}

/// Sync all repos under a GitHub user or org account.
/// Discovered repos are treated as if they were [[repo]] entries with these defaults.
#[derive(Debug, Deserialize, Clone)]
pub struct OrgConfig {
    pub owner: String,
    pub sync_prs: Option<bool>,
    pub sync_notifications: Option<bool>,
    pub sync_events: Option<bool>,
    pub sync_branches: Option<Vec<String>>,
    pub checkout_on_sync: Option<bool>,
    pub poll_interval_seconds: Option<u64>,
    /// Repo names to skip (exact match)
    #[serde(default)]
    pub exclude: Vec<String>,
    /// Short alias used as the owner directory name on disk instead of `owner`.
    pub fs_alias: Option<String>,
}

impl RepoConfig {
    pub fn slug(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }

    /// Directory name for this owner on disk (fs_alias if set, otherwise owner).
    pub fn fs_owner(&self) -> &str {
        self.fs_alias.as_deref().unwrap_or(&self.owner)
    }

    pub fn poll_interval(&self, global: &Global) -> u64 {
        self.poll_interval_seconds
            .or(global.poll_interval_seconds)
            .unwrap_or(60)
    }
}

impl OrgConfig {
    /// Directory name for this owner on disk (fs_alias if set, otherwise owner).
    pub fn fs_owner(&self) -> &str {
        self.fs_alias.as_deref().unwrap_or(&self.owner)
    }
}

/// Resolved config with all paths expanded and validated.
#[derive(Debug)]
pub struct ResolvedConfig {
    pub db_path: PathBuf,
    pub staging_folder: PathBuf,
    pub poll_interval_seconds: u64,
    pub log_level: String,
    pub gh_binary: String,
    pub rate_warn_threshold: i64,
    pub rate_stop_threshold: i64,
    pub cmd_port: u16,
    pub heartbeat_ttl_seconds: u64,
    pub sync_notifications: bool,
    pub repos: Vec<RepoConfig>,
    pub orgs: Vec<OrgConfig>,
    /// Maps GitHub owner name -> fs directory name (only entries with fs_alias set).
    pub owner_fs_aliases: std::collections::HashMap<String, String>,
}

pub fn load(explicit_path: Option<&Path>) -> Result<ResolvedConfig> {
    let path = resolve_config_path(explicit_path)?;
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("reading config from {}", path.display()))?;
    let config: Config = toml::from_str(&raw)
        .with_context(|| format!("parsing config at {}", path.display()))?;
    validate(config)
}

fn resolve_config_path(explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(p) = explicit {
        return Ok(p.to_owned());
    }
    if let Ok(v) = std::env::var("GHCACHE_CONFIG") {
        return Ok(PathBuf::from(v));
    }
    let local = PathBuf::from("ghcache.toml");
    if local.exists() {
        return Ok(local);
    }
    // XDG conventional path (what most CLI tools on macOS/Linux use)
    let xdg = dirs::home_dir().map(|h| h.join(".config").join("ghcache").join("config.toml"));
    if let Some(ref p) = xdg {
        if p.exists() {
            return Ok(p.clone());
        }
    }
    // Platform config dir (~/Library/Application Support on macOS)
    if let Some(cfg) = dirs::config_dir() {
        let platform = cfg.join("ghcache").join("config.toml");
        if platform.exists() {
            return Ok(platform);
        }
    }
    bail!("no config found; searched $GHCACHE_CONFIG, ./ghcache.toml, ~/.config/ghcache/config.toml");
}

fn validate(config: Config) -> Result<ResolvedConfig> {
    if config.repos.is_empty() && config.orgs.is_empty() {
        bail!("config must have at least one [[repo]] or [[org]] entry");
    }

    let db_path = config
        .global
        .db_path
        .as_deref()
        .unwrap_or("~/.local/share/ghcache/gh.db");
    let db_path = expand_tilde(db_path);

    if let Some(parent) = db_path.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating db_path directory {}", parent.display()))?;
        }
    }

    let staging_folder = config
        .global
        .staging_folder
        .as_deref()
        .map(expand_tilde)
        .ok_or_else(|| anyhow::anyhow!("staging_folder is required in [global] config"))?;
    if !staging_folder.exists() {
        std::fs::create_dir_all(&staging_folder)
            .with_context(|| format!("creating staging_folder {}", staging_folder.display()))?;
    }

    let mut owner_fs_aliases = std::collections::HashMap::new();
    for r in &config.repos {
        if let Some(alias) = &r.fs_alias {
            owner_fs_aliases.insert(r.owner.clone(), alias.clone());
        }
    }
    for o in &config.orgs {
        if let Some(alias) = &o.fs_alias {
            owner_fs_aliases.insert(o.owner.clone(), alias.clone());
        }
    }

    Ok(ResolvedConfig {
        db_path,
        staging_folder,
        poll_interval_seconds: config.global.poll_interval_seconds.unwrap_or(60),
        log_level: config.global.log_level.unwrap_or_else(|| "info".into()),
        gh_binary: config.global.gh_binary.unwrap_or_else(|| "gh".into()),
        rate_warn_threshold: config.global.rate_warn_threshold.unwrap_or(500),
        rate_stop_threshold: config.global.rate_stop_threshold.unwrap_or(50),
        cmd_port: config.global.cmd_port.unwrap_or(7748),
        heartbeat_ttl_seconds: config.global.heartbeat_ttl_seconds.unwrap_or(30),
        sync_notifications: config.global.sync_notifications.unwrap_or(false),
        repos: config.repos,
        orgs: config.orgs,
        owner_fs_aliases,
    })
}

fn expand_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn write_config(content: &str) -> NamedTempFile {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(content.as_bytes()).unwrap();
        f
    }

    #[test]
    fn parses_minimal_config() {
        let f = write_config(
            r#"
[global]
db_path = "/tmp/test.db"
staging_folder = "/tmp/ghcache_test_staging"

[[repo]]
owner = "myorg"
name = "backend"
"#,
        );
        let cfg = load(Some(f.path())).unwrap();
        assert_eq!(cfg.repos.len(), 1);
        assert_eq!(cfg.repos[0].owner, "myorg");
        assert_eq!(cfg.repos[0].name, "backend");
        assert_eq!(cfg.db_path, PathBuf::from("/tmp/test.db"));
    }

    #[test]
    fn fails_with_no_repos() {
        let f = write_config(
            r#"
[global]
db_path = "/tmp/test.db"
staging_folder = "/tmp/ghcache_test_staging"
"#,
        );
        let err = load(Some(f.path())).unwrap_err();
        assert!(err.to_string().contains("[[repo]]"));
    }

    #[test]
    fn fails_without_staging_folder() {
        let f = write_config(
            r#"
[global]
db_path = "/tmp/test.db"

[[repo]]
owner = "myorg"
name = "backend"
"#,
        );
        let err = load(Some(f.path())).unwrap_err();
        assert!(err.to_string().contains("staging_folder is required"));
    }

    #[test]
    fn fails_when_db_parent_missing() {
        let f = write_config(
            r#"
[global]
db_path = "/nonexistent/path/gh.db"
staging_folder = "/tmp/ghcache_test_staging"

[[repo]]
owner = "myorg"
name = "backend"
"#,
        );
        let err = load(Some(f.path())).unwrap_err();
        assert!(err.to_string().contains("creating db_path directory"));
    }

    #[test]
    fn repo_slug() {
        let r = RepoConfig {
            owner: "myorg".into(),
            name: "backend".into(),
            default_branch: None,
            sync_prs: None,
            sync_notifications: None,
            sync_events: None,
            sync_branches: None,
            checkout_on_sync: None,
            poll_interval_seconds: None,
            fs_alias: None,
        };
        assert_eq!(r.slug(), "myorg/backend");
    }

    #[test]
    fn poll_interval_falls_back_to_global() {
        let global = Global {
            poll_interval_seconds: Some(120),
            ..Default::default()
        };
        let r = RepoConfig {
            owner: "o".into(),
            name: "n".into(),
            default_branch: None,
            sync_prs: None,
            sync_notifications: None,
            sync_events: None,
            sync_branches: None,
            checkout_on_sync: None,
            poll_interval_seconds: None,
            fs_alias: None,
        };
        assert_eq!(r.poll_interval(&global), 120);
    }

    #[test]
    fn poll_interval_repo_overrides_global() {
        let global = Global {
            poll_interval_seconds: Some(120),
            ..Default::default()
        };
        let r = RepoConfig {
            owner: "o".into(),
            name: "n".into(),
            default_branch: None,
            sync_prs: None,
            sync_notifications: None,
            sync_events: None,
            sync_branches: None,
            checkout_on_sync: None,
            poll_interval_seconds: Some(30),
            fs_alias: None,
        };
        assert_eq!(r.poll_interval(&global), 30);
    }

    #[test]
    fn parses_multi_repo_config() {
        let f = write_config(
            r#"
[global]
db_path = "/tmp/test.db"
staging_folder = "/tmp/ghcache_test_staging"
poll_interval_seconds = 60

[[repo]]
owner = "myorg"
name = "backend"
sync_prs = true
sync_branches = ["main", "staging", "release/*"]

[[repo]]
owner = "myorg"
name = "frontend"
default_branch = "main"
sync_prs = true
poll_interval_seconds = 30
"#,
        );
        let cfg = load(Some(f.path())).unwrap();
        assert_eq!(cfg.repos.len(), 2);
        assert_eq!(cfg.repos[1].name, "frontend");
        assert_eq!(cfg.repos[1].poll_interval_seconds, Some(30));
        assert_eq!(
            cfg.repos[0].sync_branches.as_ref().map(|v| v.iter().map(|s| s.as_str()).collect::<Vec<_>>()),
            Some(vec!["main", "staging", "release/*"])
        );
    }
}
