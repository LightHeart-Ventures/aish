//! The GitHub edge: three requests, one retry policy, zero business logic
//! (TASK-695).
//!
//! | Call | Endpoint | Why |
//! |---|---|---|
//! | [`GitHubClient::repo_meta`] | `GET /repos/{o}/{r}` | star count (the ranking signal) + default branch |
//! | [`GitHubClient::tree`] | `GET /repos/{o}/{r}/git/trees/{ref}?recursive=1` | find every SKILL.md in **one** call, with sizes, conditionally |
//! | [`GitHubClient::raw`] | `raw.githubusercontent.com/{o}/{r}/{ref}/{path}` | the verbatim bytes |
//!
//! Three deliberate choices:
//!
//! * **The recursive tree call, not the Contents API.** One request enumerates
//!   the whole repo *with blob sizes*, which lets ingest reject an oversize
//!   SKILL.md without ever downloading it.
//! * **Conditional requests.** The caller replays the stored ETag; a 304 is
//!   free (GitHub does not bill it against the rate limit) and short-circuits
//!   the entire repo.
//! * **Both base URLs are injectable.** The API and raw hosts are constructor
//!   arguments, which is what makes the ingest path testable against a loopback
//!   server instead of the real GitHub.
//!
//! Rate limits are honoured rather than hammered: a 403/429 carrying
//! `retry-after`, or `x-ratelimit-remaining: 0` plus `x-ratelimit-reset`, parks
//! the request for exactly that long (capped), and anything else retryable
//! walks an exponential backoff.

use anyhow::{Context, Result, bail};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const UA: &str = concat!("aish-skill-mirror/", env!("CARGO_PKG_VERSION"));
const API_ACCEPT: &str = "application/vnd.github+json";

/// What `GET /repos/{owner}/{repo}` tells us that we actually use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoMeta {
    pub stars: u64,
    pub default_branch: String,
}

/// One blob in a repo tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Blob {
    pub path: String,
    pub size: u64,
}

/// Outcome of a conditional tree request.
#[derive(Debug, Clone)]
pub enum TreeResponse {
    /// The ETag matched: nothing in the repo changed since the last crawl.
    NotModified,
    Tree {
        sha: String,
        etag: Option<String>,
        blobs: Vec<Blob>,
        /// GitHub truncates trees past ~100k entries; a truncated tree may be
        /// missing SKILL.md files, which the caller surfaces as a WARN.
        truncated: bool,
    },
}

/// A thin, retrying GitHub client with injectable hosts.
pub struct GitHubClient {
    http: reqwest::Client,
    api_base: String,
    raw_base: String,
    token: Option<String>,
    max_retries: u32,
    max_backoff: Duration,
}

impl GitHubClient {
    pub fn new(api_base: &str, raw_base: &str, token: Option<String>) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(UA)
            .timeout(Duration::from_secs(60))
            .build()
            .context("building the HTTP client")?;
        Ok(Self {
            http,
            api_base: api_base.trim_end_matches('/').to_string(),
            raw_base: raw_base.trim_end_matches('/').to_string(),
            token: token.filter(|t| !t.trim().is_empty()),
            max_retries: 4,
            max_backoff: Duration::from_secs(60),
        })
    }

    /// Tighten the retry envelope (tests use milliseconds, not minutes).
    pub fn with_retry_limits(mut self, max_retries: u32, max_backoff: Duration) -> Self {
        self.max_retries = max_retries;
        self.max_backoff = max_backoff;
        self
    }

    /// True when a token is in play — logged once so a nightly run makes its
    /// 5000/hr vs 60/hr budget obvious in the job output.
    pub fn authenticated(&self) -> bool {
        self.token.is_some()
    }

    async fn get(
        &self,
        url: &str,
        accept: &str,
        if_none_match: Option<&str>,
    ) -> Result<reqwest::Response> {
        let mut attempt = 0u32;
        loop {
            let mut req = self.http.get(url).header(reqwest::header::ACCEPT, accept);
            if let Some(t) = &self.token {
                req = req.header(reqwest::header::AUTHORIZATION, format!("Bearer {t}"));
                // Pin the REST API version for the token path; harmless on raw.
                req = req.header("x-github-api-version", "2022-11-28");
            }
            if let Some(etag) = if_none_match {
                req = req.header(reqwest::header::IF_NONE_MATCH, etag);
            }

            let resp = match req.send().await {
                Ok(r) => r,
                Err(e) => {
                    if attempt >= self.max_retries {
                        return Err(e).with_context(|| format!("GET {url}"));
                    }
                    self.backoff(attempt, None).await;
                    attempt += 1;
                    continue;
                }
            };

            let code = resp.status().as_u16();
            let retryable = code == 403 || code == 429 || (500..600).contains(&code);
            if retryable && attempt < self.max_retries {
                let hint = retry_hint(&resp);
                eprintln!(
                    "skill-mirror: WARN {code} from {url}; retry {}/{} in {:?}",
                    attempt + 1,
                    self.max_retries,
                    hint.unwrap_or_else(|| backoff_for(attempt))
                        .min(self.max_backoff)
                );
                self.backoff(attempt, hint).await;
                attempt += 1;
                continue;
            }
            return Ok(resp);
        }
    }

    async fn backoff(&self, attempt: u32, hint: Option<Duration>) {
        let d = hint
            .unwrap_or_else(|| backoff_for(attempt))
            .min(self.max_backoff);
        tokio::time::sleep(d).await;
    }

    /// Star count + default branch.
    pub async fn repo_meta(&self, owner: &str, repo: &str) -> Result<RepoMeta> {
        let url = format!("{}/repos/{owner}/{repo}", self.api_base);
        let resp = self.get(&url, API_ACCEPT, None).await?;
        let status = resp.status();
        if !status.is_success() {
            bail!("GET /repos/{owner}/{repo} returned {status}");
        }
        let v: serde_json::Value = resp
            .json()
            .await
            .with_context(|| format!("decoding /repos/{owner}/{repo}"))?;
        Ok(RepoMeta {
            stars: v
                .get("stargazers_count")
                .and_then(|s| s.as_u64())
                .unwrap_or(0),
            default_branch: v
                .get("default_branch")
                .and_then(|s| s.as_str())
                .unwrap_or("HEAD")
                .to_string(),
        })
    }

    /// The full recursive tree for `git_ref`, conditionally on `etag`.
    pub async fn tree(
        &self,
        owner: &str,
        repo: &str,
        git_ref: &str,
        etag: Option<&str>,
    ) -> Result<TreeResponse> {
        let url = format!(
            "{}/repos/{owner}/{repo}/git/trees/{git_ref}?recursive=1",
            self.api_base
        );
        let resp = self.get(&url, API_ACCEPT, etag).await?;
        if resp.status().as_u16() == 304 {
            return Ok(TreeResponse::NotModified);
        }
        let status = resp.status();
        if !status.is_success() {
            bail!("GET tree {owner}/{repo}@{git_ref} returned {status}");
        }
        let etag = resp
            .headers()
            .get(reqwest::header::ETAG)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let v: serde_json::Value = resp
            .json()
            .await
            .with_context(|| format!("decoding tree {owner}/{repo}@{git_ref}"))?;
        let sha = v
            .get("sha")
            .and_then(|s| s.as_str())
            .unwrap_or_default()
            .to_string();
        let truncated = v
            .get("truncated")
            .and_then(|t| t.as_bool())
            .unwrap_or(false);
        let mut blobs: Vec<Blob> = v
            .get("tree")
            .and_then(|t| t.as_array())
            .map(|rows| {
                rows.iter()
                    .filter(|r| r.get("type").and_then(|t| t.as_str()) == Some("blob"))
                    .filter_map(|r| {
                        Some(Blob {
                            path: r.get("path")?.as_str()?.to_string(),
                            size: r.get("size").and_then(|s| s.as_u64()).unwrap_or(0),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        // Byte-stable downstream output starts with a stable walk order.
        blobs.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(TreeResponse::Tree {
            sha,
            etag,
            blobs,
            truncated,
        })
    }

    /// Verbatim bytes of one path at one ref.
    pub async fn raw(&self, owner: &str, repo: &str, git_ref: &str, path: &str) -> Result<Vec<u8>> {
        let url = format!("{}/{owner}/{repo}/{git_ref}/{path}", self.raw_base);
        let resp = self.get(&url, "*/*", None).await?;
        let status = resp.status();
        if !status.is_success() {
            bail!("GET raw {owner}/{repo}@{git_ref}:{path} returned {status}");
        }
        let bytes = resp
            .bytes()
            .await
            .with_context(|| format!("reading raw {owner}/{repo}:{path}"))?;
        Ok(bytes.to_vec())
    }
}

/// `200ms, 400ms, 800ms, …`
fn backoff_for(attempt: u32) -> Duration {
    Duration::from_millis(200u64.saturating_mul(1u64 << attempt.min(8)))
}

/// How long the server told us to wait, if it did.
///
/// `retry-after` (seconds) wins; otherwise an exhausted `x-ratelimit-remaining`
/// plus `x-ratelimit-reset` (unix seconds) gives the exact reset instant.
fn retry_hint(resp: &reqwest::Response) -> Option<Duration> {
    let h = resp.headers();
    if let Some(secs) = h
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok())
    {
        return Some(Duration::from_secs(secs));
    }
    let remaining = h
        .get("x-ratelimit-remaining")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok())?;
    if remaining > 0 {
        return None;
    }
    let reset = h
        .get("x-ratelimit-reset")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok())?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    Some(Duration::from_secs(reset.saturating_sub(now).max(1)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_then_is_capped_by_caller() {
        assert_eq!(backoff_for(0), Duration::from_millis(200));
        assert_eq!(backoff_for(1), Duration::from_millis(400));
        assert_eq!(backoff_for(2), Duration::from_millis(800));
        // The cap is applied at the call site; the curve itself just grows.
        assert!(backoff_for(6) > backoff_for(5));
    }

    #[test]
    fn base_urls_lose_their_trailing_slash() {
        let c = GitHubClient::new("https://api.example/", "https://raw.example/", None).unwrap();
        assert_eq!(c.api_base, "https://api.example");
        assert_eq!(c.raw_base, "https://raw.example");
        assert!(!c.authenticated());
        assert!(
            GitHubClient::new("https://a", "https://b", Some("  ".into()))
                .unwrap()
                .authenticated()
                .eq(&false),
            "a blank token must not count as authenticated"
        );
    }
}
