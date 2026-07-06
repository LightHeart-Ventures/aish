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
                // Delegate to the existing in-process search path.
                // (This is a placeholder; real impl calls skill_provider::search.)
                crate::skill_provider::search(query).await
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
                // Placeholder; real impl calls skill_provider::add(reference).
                // Returns the imported skill metadata.
                bail!("builtin add not yet implemented")
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
}
