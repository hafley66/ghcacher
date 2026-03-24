use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::process::Command;
use std::time::Instant;

use crate::db;

pub struct GhClient {
    pub binary: String,
}

pub struct GhRequest<'a> {
    pub endpoint: &'a str,
    pub method: &'a str,
    pub headers: Vec<(&'a str, &'a str)>,
    pub body: Option<&'a str>,
    pub paginate: bool,
}

impl<'a> GhRequest<'a> {
    pub fn get(endpoint: &'a str) -> Self {
        GhRequest {
            endpoint,
            method: "GET",
            headers: vec![],
            body: None,
            paginate: false,
        }
    }

    pub fn with_etag(mut self, etag: &'a str) -> Self {
        self.headers.push(("If-None-Match", etag));
        self
    }

    pub fn with_last_modified(mut self, lm: &'a str) -> Self {
        self.headers.push(("If-Modified-Since", lm));
        self
    }

    pub fn paginated(mut self) -> Self {
        self.paginate = true;
        self
    }
}

pub struct GhResponse {
    pub status: u16,
    pub headers: HashMap<String, String>,
    pub body: serde_json::Value,
    #[allow(dead_code)]
    pub duration_ms: u64,
}

impl GhResponse {
    pub fn is_not_modified(&self) -> bool {
        self.status == 304
    }

    pub fn etag(&self) -> Option<&str> {
        self.headers.get("etag").map(|s| s.as_str())
    }

    pub fn last_modified(&self) -> Option<&str> {
        self.headers.get("last-modified").map(|s| s.as_str())
    }

    pub fn poll_interval(&self) -> Option<i64> {
        self.headers
            .get("x-poll-interval")
            .and_then(|v| v.parse().ok())
    }

    pub fn rate_remaining(&self) -> Option<i64> {
        self.headers
            .get("x-ratelimit-remaining")
            .and_then(|v| v.parse().ok())
    }

    pub fn rate_reset(&self) -> Option<i64> {
        self.headers
            .get("x-ratelimit-reset")
            .and_then(|v| v.parse().ok())
    }
}

impl GhClient {
    pub fn new(binary: impl Into<String>) -> Self {
        GhClient { binary: binary.into() }
    }

    pub fn call(&self, conn: &rusqlite::Connection, req: &GhRequest) -> Result<GhResponse> {
        let mut cmd = Command::new(&self.binary);
        cmd.arg("api").arg("--include");

        if req.method != "GET" {
            cmd.arg("-X").arg(req.method);
        }
        if req.paginate {
            cmd.arg("--paginate");
        }
        for (k, v) in &req.headers {
            cmd.arg("-H").arg(format!("{}: {}", k, v));
        }
        if let Some(body) = req.body {
            cmd.arg("--input").arg("-");
            cmd.stdin(std::process::Stdio::piped());
            let _ = body; // body passed via stdin below
        }

        cmd.arg(req.endpoint);

        let t = Instant::now();

        let output = if req.body.is_some() {
            let mut child = cmd.stdin(std::process::Stdio::piped()).spawn()
                .context("spawning gh")?;
            use std::io::Write;
            child.stdin.as_mut().unwrap().write_all(req.body.unwrap().as_bytes())?;
            child.wait_with_output().context("waiting on gh")?
        } else {
            cmd.output().context("spawning gh")?
        };

        let duration_ms = t.elapsed().as_millis() as u64;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let (status, headers, body_str) = parse_response(&stdout)?;

        let body: serde_json::Value = if body_str.trim().is_empty() || status == 304 {
            serde_json::Value::Null
        } else if req.paginate {
            // --paginate concatenates JSON arrays; wrap them
            parse_paginated_body(body_str)?
        } else {
            serde_json::from_str(body_str)
                .with_context(|| format!("parsing JSON body for {}", req.endpoint))?
        };

        let resp = GhResponse { status, headers, body, duration_ms };

        db::log_call(
            conn,
            &db::CallLogEntry {
                endpoint: req.endpoint,
                method: req.method,
                status_code: Some(status),
                etag: resp.etag(),
                last_modified: resp.last_modified(),
                rate_remaining: resp.rate_remaining(),
                rate_reset: resp.rate_reset(),
                cache_hit: resp.is_not_modified(),
                duration_ms: Some(duration_ms as i64),
            },
        )?;

        db::set_poll_state(
            conn,
            req.endpoint,
            resp.etag(),
            resp.last_modified(),
            resp.poll_interval(),
            !resp.is_not_modified(),
        )?;

        Ok(resp)
    }

    pub fn graphql(&self, conn: &rusqlite::Connection, query: &str) -> Result<serde_json::Value> {
        let mut cmd = Command::new(&self.binary);
        cmd.arg("api").arg("graphql").arg("--include").arg("-f")
            .arg(format!("query={}", query));

        let t = Instant::now();
        let output = cmd.output().context("spawning gh for graphql")?;
        let duration_ms = t.elapsed().as_millis() as u64;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let (status, headers, body_str) = parse_response(&stdout)?;

        if status != 200 {
            bail!("GraphQL request failed with status {}: {}", status, body_str);
        }

        let body: serde_json::Value = serde_json::from_str(body_str)
            .context("parsing GraphQL JSON response")?;

        if let Some(errors) = body.get("errors") {
            bail!("GraphQL errors: {}", errors);
        }

        db::log_call(
            conn,
            &db::CallLogEntry {
                endpoint: "graphql",
                method: "POST",
                status_code: Some(status),
                etag: headers.get("etag").map(|s| s.as_str()),
                last_modified: None,
                rate_remaining: headers.get("x-ratelimit-remaining").and_then(|v| v.parse().ok()),
                rate_reset: headers.get("x-ratelimit-reset").and_then(|v| v.parse().ok()),
                cache_hit: false,
                duration_ms: Some(duration_ms as i64),
            },
        )?;

        Ok(body["data"].clone())
    }
}

/// Parse the `--include` output: HTTP status line + headers, blank line, body.
fn parse_response(raw: &str) -> Result<(u16, HashMap<String, String>, &str)> {
    // Find the last HTTP/... block (paginate appends multiple responses)
    let last_http = raw.rfind("\nHTTP/").map(|i| i + 1).unwrap_or(0);
    let relevant = &raw[last_http..];

    let sep = relevant.find("\r\n\r\n").or_else(|| relevant.find("\n\n"));
    let (head, body) = match sep {
        Some(i) => {
            let eol_len = if relevant[i..].starts_with("\r\n\r\n") { 4 } else { 2 };
            (&relevant[..i], &relevant[i + eol_len..])
        }
        None => bail!("malformed gh response: no header/body separator"),
    };

    let mut lines = head.lines();
    let status_line = lines.next().context("empty response")?;
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .context("no status code in response line")?
        .parse()
        .context("parsing status code")?;

    let mut headers = HashMap::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            headers.insert(k.trim().to_lowercase(), v.trim().to_owned());
        }
    }

    Ok((status, headers, body))
}

/// `gh api --paginate` streams multiple JSON arrays; concatenate into one.
fn parse_paginated_body(body: &str) -> Result<serde_json::Value> {
    let mut combined: Vec<serde_json::Value> = vec![];
    // Pages are separated by newlines; each is a JSON array
    for chunk in body.split('\n') {
        let chunk = chunk.trim();
        if chunk.is_empty() { continue; }
        let val: serde_json::Value = serde_json::from_str(chunk)
            .with_context(|| format!("parsing paginated chunk: {}", &chunk[..chunk.len().min(100)]))?;
        if let serde_json::Value::Array(arr) = val {
            combined.extend(arr);
        }
    }
    Ok(serde_json::Value::Array(combined))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_200_response() {
        let raw = "HTTP/2 200\r\ncontent-type: application/json\r\netag: \"abc123\"\r\nX-RateLimit-Remaining: 4998\r\n\r\n{\"foo\":1}";
        let (status, headers, body) = parse_response(raw).unwrap();
        assert_eq!(status, 200);
        assert_eq!(headers.get("etag").map(|s| s.as_str()), Some("\"abc123\""));
        assert_eq!(headers.get("x-ratelimit-remaining").map(|s| s.as_str()), Some("4998"));
        assert_eq!(body, "{\"foo\":1}");
    }

    #[test]
    fn parse_304_response() {
        let raw = "HTTP/2 304\r\n\r\n";
        let (status, _, body) = parse_response(raw).unwrap();
        assert_eq!(status, 304);
        assert_eq!(body, "");
    }

    #[test]
    fn parse_lf_only_response() {
        let raw = "HTTP/2 200\ncontent-type: application/json\n\n{\"x\":2}";
        let (status, _, body) = parse_response(raw).unwrap();
        assert_eq!(status, 200);
        assert_eq!(body, "{\"x\":2}");
    }

    #[test]
    fn parse_paginated_body_merges_arrays() {
        let body = "[{\"id\":1},{\"id\":2}]\n[{\"id\":3}]\n";
        let val = parse_paginated_body(body).unwrap();
        let arr = val.as_array().unwrap();
        assert_eq!(arr.len(), 3);
        assert_eq!(arr[2]["id"], 3);
    }

    #[test]
    fn parse_paginated_body_empty() {
        let val = parse_paginated_body("").unwrap();
        assert_eq!(val.as_array().unwrap().len(), 0);
    }

    #[test]
    fn gh_response_helpers() {
        let raw = "HTTP/2 200\r\nETag: \"xyz\"\r\nLast-Modified: Thu, 01 Jan 2026 00:00:00 GMT\r\nX-Poll-Interval: 60\r\nX-RateLimit-Remaining: 100\r\nX-RateLimit-Reset: 1700000000\r\n\r\nnull";
        let (status, headers, body_str) = parse_response(raw).unwrap();
        let resp = GhResponse {
            status,
            headers,
            body: serde_json::from_str(body_str).unwrap(),
            duration_ms: 10,
        };
        assert_eq!(resp.etag(), Some("\"xyz\""));
        assert_eq!(resp.last_modified(), Some("Thu, 01 Jan 2026 00:00:00 GMT"));
        assert_eq!(resp.poll_interval(), Some(60));
        assert_eq!(resp.rate_remaining(), Some(100));
        assert_eq!(resp.rate_reset(), Some(1700000000));
        assert!(!resp.is_not_modified());
    }

    #[test]
    fn parse_picks_last_http_block_for_paginate() {
        // Simulate two pages concatenated by --paginate --include
        let raw = "HTTP/2 200\r\netag: \"first\"\r\n\r\n[{\"id\":1}]\nHTTP/2 200\r\netag: \"second\"\r\n\r\n[{\"id\":2}]";
        let (status, headers, _) = parse_response(raw).unwrap();
        assert_eq!(status, 200);
        assert_eq!(headers.get("etag").map(|s| s.as_str()), Some("\"second\""));
    }
}
