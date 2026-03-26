use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PullRequest {
    pub id: i64,
    pub repo_slug: String,
    pub number: i64,
    pub title: String,
    pub author: Option<String>,
    pub head_ref: Option<String>,
    pub base_ref: Option<String>,
    pub state: String,
    pub draft: bool,
    pub mergeable: Option<String>,
    pub additions: Option<i64>,
    pub deletions: Option<i64>,
    pub changed_files: Option<i64>,
    pub created_at: String,
    pub updated_at: String,
    pub merged_at: Option<String>,
    pub body: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Review {
    pub id: i64,
    pub pr_id: i64,
    pub author: Option<String>,
    pub state: String,
    pub body: Option<String>,
    pub submitted_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Comment {
    pub id: i64,
    pub pr_id: i64,
    pub author: Option<String>,
    pub body: String,
    pub path: Option<String>,
    pub line: Option<i64>,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Notification {
    pub id: i64,
    pub gh_id: String,
    pub repo_slug: Option<String>,
    pub subject_type: String,
    pub subject_title: String,
    pub subject_url: Option<String>,
    pub subject_number: Option<i64>,
    pub html_url: Option<String>,
    pub reason: String,
    pub unread: bool,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Branch {
    pub id: i64,
    pub repo_slug: String,
    pub name: String,
    pub sha: Option<String>,
    pub behind_default: Option<i64>,
    pub ahead_default: Option<i64>,
    pub updated_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoEvent {
    pub id: i64,
    pub repo_slug: String,
    pub gh_id: String,
    pub event_type: String,
    pub actor: Option<String>,
    pub payload: serde_json::Value,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RateLimit {
    pub api_type: String,
    pub remaining: Option<i64>,
    pub reset_at: Option<String>,
    pub gql_cost: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Checkout {
    pub repo_slug: String,
    pub branch: String,
    pub local_path: String,
    pub sha: Option<String>,
    pub checked_out_at: String,
}

/// Filters for PR queries.
#[derive(Debug, Default)]
pub struct PrFilter<'a> {
    pub repo: Option<&'a str>,
    pub state: Option<&'a str>,
    pub needs_review: bool,
    pub author: Option<&'a str>,
}
