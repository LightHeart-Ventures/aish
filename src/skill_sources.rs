//! SkillSource façade — unified interface for search/add across in-process and
//! script-based skill sources.
//!
//! Per `docs/design/plugin-skill-sources.md` §5–§6, exposes two variants:
//! - **Builtin**: in-process, calls `skill_provider::search` / `skill_provider::add`
//!   (reserved handler `"@builtin"`).
//! - **Script**: runs `search.sh` / `add.sh` via `run_plugin_handler` from
//!   `plugin_auth.rs`, with the handler contract (env in, JSON stdout, non-zero=error).
//!
//! Both expose a uniform `search(query) -> Result<Vec<SearchResult>>` and
//! `add(ref) -> Result<Vec<SkillMetadata>>` interface. Imported skills are
//! persisted via the existing `skill_provider::import` path; this module handles
//! *discovery* only.

// The façade lands ahead of its REPL consumer: the `:skill` command wiring that
// selects a `SkillSource` and calls `search`/`add` is a follow-up task per
// docs/design/plugin-skill-sources.md §7 (rollout). Until that lands the public
// surface is exercised only by this module's tests, so scope-suppress the
// not-yet-wired dead_code lint at the module boundary rather than sprinkling
// per-item allows.
#![allow(dead_code)]

use anyhow::{Context, Result, bail};
use crate::skill_provider::SearchResult;
use serde::Deserialize;
use std::path::PathBuf;

/// A skill metadata record returned by an `add` handler (per design §3.1).
/// Mirrors the shape a script outputs, and what `skill_provider::import` expects
/// when persisting fetched skills.
#[derive(Debug, Clone, Deserialize)]
pub struct SkillMetadata {
    /// Relative path under AISH_SKILLS_DIR (e.g. "hello-world" or "team/task-runner").
    /// When the handler returns a single SKILL.md as raw text, the REPL side
    /// synthesizes `{ "path": "<skill-name>", "content": "<SKILL.md>" }`.
    #[serde(default)]
    pub path: String,
    /// The raw SKILL.md body to write to disk.
    #[serde(default)]
    pub content: String,
}

/// A skill source — either in-process (builtin) or script-based (plugin).
/// Both expose `search` and `add` methods that return the same types, hiding the
/// transport details from the caller.
#[derive(Debug, Clone)]
pub enum SkillSource {
    /// In-process: delegates to `skill_provider::search` / `skill_provider::add`.
    /// Used as the fallback source and for testing.
    Builtin,
    /// Script: runs `search.sh` or `add.sh` in a plugin's directory via
    /// `run_plugin_handler` with a curated env (design §3.1).
    Script {
        plugin_dir: PathBuf,
    },
}

impl SkillSource {
    /// Search for skills matching a query.
    /// Returns a `Vec<SearchResult>` (zero results is not an error).
    /// On script error (non-zero exit, JSON parse failure), returns the error.
    pub async fn search(&self, query: &str) -> Result<Vec<SearchResult>> {
        match self {
            SkillSource::Builtin => {
                // In-process path (reserved handler "@builtin", design §5):
                // delegate to the built-in *core* source (offline embedded index).
                // We call `search_core` rather than `search` so this leaf does
                // not itself re-fan out to plugins — the federation fan-out lives
                // one layer up (the repl `:skill search` orchestration and the
                // public `skill_provider::search`), and double-fanning would
                // double-execute plugin handlers.
                crate::skill_provider::search_core(query).await
            }
            SkillSource::Script { plugin_dir } => {
                let script = plugin_dir.join("search.sh");
                if !script.exists() {
                    // Search is optional; no handler means no results (not an error).
                    return Ok(Vec::new());
                }

                let env = [
                    ("AISH_SKILL_QUERY", query.to_string()),
                    // AISH_SKILL_LIMIT, AISH_PLUGIN_ID, AISH_TENANT_ID, AISH_CREDENTIALS_FILE
                    // would be set by the caller; we defer to the handler contract.
                ];

                let output = crate::plugin_auth::run_plugin_handler(&script, plugin_dir, &env)?;
                parse_search_result(&output)
            }
        }
    }

    /// Add a skill by reference, returning its SKILL.md content(s).
    /// The `add` handler returns either:
    /// - Raw SKILL.md text (single skill) → synthesized as `{ path: "<name>", content: "..." }`
    /// - JSON array of `{ path, content }` objects (multi-skill import)
    pub async fn add(&self, reference: &str) -> Result<Vec<SkillMetadata>> {
        match self {
            SkillSource::Builtin => {
                // In-process path (reserved handler "@builtin", design §5): import
                // directly via skill_provider::add, which detects the source, fetches
                // and writes the skill under the default skills dir, then map each
                // imported skill to the uniform SkillMetadata shape (reading the
                // persisted SKILL.md back so the return type matches the script path).
                let skills_dir = default_skills_dir();
                let imported = crate::skill_provider::add(reference, &skills_dir).await?;
                Ok(imported
                    .into_iter()
                    .map(|s| SkillMetadata {
                        content: std::fs::read_to_string(&s.path).unwrap_or_default(),
                        path: s.name,
                    })
                    .collect())
            }
            SkillSource::Script { plugin_dir } => {
                let script = plugin_dir.join("add.sh");
                if !script.exists() {
                    // Add is optional; no handler means unsupported (not an error at this layer).
                    bail!("add handler not found for {}", plugin_dir.display())
                }

                let env = [
                    ("AISH_SKILL_REF", reference.to_string()),
                    // AISH_PLUGIN_ID, AISH_SKILLS_DIR, AISH_CREDENTIALS_FILE would be set by caller.
                ];

                let output =
                    crate::plugin_auth::run_plugin_handler(&script, plugin_dir, &env)?;
                parse_add_result(&output, reference)
            }
        }
    }
}

/// Default skills directory (`~/.aish/skills`), mirroring `skill_provider`'s
/// internal default. Used by the `Builtin` variant when the caller does not
/// supply an explicit dir. The REPL fall-through (design §4.2) may instead call
/// `skill_provider::add` directly with its own known skills dir.
fn default_skills_dir() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default())
        .join(".aish")
        .join("skills")
}

/// Parse the stdout of a search handler.
/// Expected to be a JSON array of SearchResult objects (per design §3.1).
fn parse_search_result(output: &str) -> Result<Vec<SearchResult>> {
    let trimmed = output.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    serde_json::from_str(trimmed).context("failed to parse search handler JSON output")
}

/// Parse the stdout of an add handler.
/// Expected to be EITHER:
/// - A raw SKILL.md (text) → synthesized as a single SkillMetadata with the reference as the path.
/// - A JSON array of { path, content } objects.
fn parse_add_result(output: &str, reference: &str) -> Result<Vec<SkillMetadata>> {
    let trimmed = output.trim();
    if trimmed.is_empty() {
        bail!("add handler returned empty output for {}", reference);
    }

    // Try JSON array first.
    if trimmed.starts_with('[') {
        return serde_json::from_str::<Vec<SkillMetadata>>(trimmed)
            .context("failed to parse add handler JSON array");
    }

    // Try JSON object (single skill).
    if trimmed.starts_with('{') {
        let metadata: SkillMetadata = serde_json::from_str(trimmed)
            .context("failed to parse add handler JSON object")?;
        return Ok(vec![metadata]);
    }

    // Otherwise, treat as raw SKILL.md text and synthesize metadata.
    // Extract the skill name from the reference (e.g. "owner/skill-name" → "skill-name").
    let skill_name = reference
        .split('/')
        .last()
        .unwrap_or(reference)
        .to_string();

    Ok(vec![SkillMetadata {
        path: skill_name,
        content: output.to_string(),
    }])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_builtin_variant_creation() {
        let source = SkillSource::Builtin;
        match source {
            SkillSource::Builtin => {}
            _ => panic!("expected Builtin variant"),
        }
    }

    #[test]
    fn test_script_variant_creation() {
        let plugin_dir = PathBuf::from("/home/user/.aish/plugins/example");
        let source = SkillSource::Script {
            plugin_dir: plugin_dir.clone(),
        };
        match source {
            SkillSource::Script { plugin_dir: pd } => {
                assert_eq!(pd, plugin_dir);
            }
            _ => panic!("expected Script variant"),
        }
    }

    #[test]
    fn test_parse_search_result_empty() {
        let result = parse_search_result("").unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn test_parse_search_result_json_array() {
        let json = r#"[
            {
                "name": "hello-world",
                "author": "acme",
                "description": "A simple example",
                "version": "1.0.0",
                "reference": "acme/hello-world",
                "stars": 42
            }
        ]"#;
        let result = parse_search_result(json).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].name, "hello-world");
        assert_eq!(result[0].author, "acme");
        assert_eq!(result[0].stars, 42);
    }

    #[test]
    fn test_parse_search_result_invalid_json() {
        let json = r#"{ invalid json"#;
        let result = parse_search_result(json);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_add_result_raw_skill_md() {
        let skill_md = "# My Skill\n\nA helpful skill.\n";
        let result = parse_add_result(skill_md, "author/my-skill").unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].path, "my-skill");
        assert_eq!(result[0].content, skill_md);
    }

    #[test]
    fn test_parse_add_result_json_single() {
        let json = "{\n            \"path\": \"custom-path\",\n            \"content\": \"# Skill\\n\\nContent here.\"\n        }";
        let result = parse_add_result(json, "author/skill").unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].path, "custom-path");
    }

    #[test]
    fn test_parse_add_result_json_array() {
        let json = "[\n            {\n                \"path\": \"skill-a\",\n                \"content\": \"# Skill A\"\n            },\n            {\n                \"path\": \"team/skill-b\",\n                \"content\": \"# Skill B\"\n            }\n        ]";
        let result = parse_add_result(json, "author/bundle").unwrap();
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].path, "skill-a");
        assert_eq!(result[1].path, "team/skill-b");
    }

    #[test]
    fn test_parse_add_result_empty() {
        let result = parse_add_result("", "author/skill");
        assert!(result.is_err());
    }

    #[test]
    fn test_default_skills_dir_shape() {
        // Builtin variant resolves the standard ~/.aish/skills location.
        let dir = default_skills_dir();
        assert!(dir.ends_with("skills"));
        assert!(dir.to_string_lossy().contains(".aish"));
    }

    /// Create a unique temp dir for a script-handler fixture.
    fn unique_tmp_dir(tag: &str) -> PathBuf {
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("{tag}_{}_{}", std::process::id(), nanos))
    }

    /// Write an executable handler script into `dir`.
    #[cfg(unix)]
    fn write_handler(dir: &std::path::Path, name: &str, body: &str) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(dir).unwrap();
        let script = dir.join(name);
        std::fs::write(&script, body).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_script_search_executes_handler() {
        let dir = unique_tmp_dir("skillsrc_search_ok");
        write_handler(
            &dir,
            "search.sh",
            "#!/bin/sh\ncat <<'EOF'\n[{\"name\":\"demo\",\"author\":\"acme\",\"description\":\"d\",\"version\":\"1.0.0\",\"reference\":\"acme/demo\",\"stars\":7}]\nEOF\n",
        );
        let src = SkillSource::Script {
            plugin_dir: dir.clone(),
        };
        let res = src.search("anything").await.unwrap();
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].name, "demo");
        assert_eq!(res[0].stars, 7);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_script_search_nonzero_exit_errors() {
        let dir = unique_tmp_dir("skillsrc_search_fail");
        write_handler(&dir, "search.sh", "#!/bin/sh\necho boom >&2\nexit 3\n");
        let src = SkillSource::Script {
            plugin_dir: dir.clone(),
        };
        // Non-zero exit from run_plugin_handler propagates as an error.
        assert!(src.search("q").await.is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_script_search_missing_handler_is_empty() {
        // No search.sh present → optional handler → empty results, not an error.
        let dir = unique_tmp_dir("skillsrc_search_none");
        std::fs::create_dir_all(&dir).unwrap();
        let src = SkillSource::Script {
            plugin_dir: dir.clone(),
        };
        let res = src.search("q").await.unwrap();
        assert!(res.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_script_add_executes_handler() {
        // add.sh emits a raw SKILL.md → synthesized into a single SkillMetadata,
        // with the path derived from the reference's trailing segment.
        let dir = unique_tmp_dir("skillsrc_add_ok");
        write_handler(&dir, "add.sh", "#!/bin/sh\nprintf '# Demo Skill\\n\\nbody\\n'\n");
        let src = SkillSource::Script {
            plugin_dir: dir.clone(),
        };
        let res = src.add("acme/demo").await.unwrap();
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].path, "demo");
        assert!(res[0].content.contains("Demo Skill"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_script_add_missing_handler_errors() {
        let dir = unique_tmp_dir("skillsrc_add_none");
        std::fs::create_dir_all(&dir).unwrap();
        let src = SkillSource::Script {
            plugin_dir: dir.clone(),
        };
        assert!(src.add("acme/demo").await.is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
