//! Skills — reusable instruction packs from two sources, one catalog.
//!
//! Local: ~/.aish/skills/<name>/SKILL.md in the Claude-skill convention —
//! YAML frontmatter with `name:` and `description:`, then a markdown body.
//! The model reads the SKILL.md (and anything it references) with read_file
//! when a task matches.
//!
//! MCP: servers publish skills as MCP prompts (`prompts/list`); mcp.rs
//! fetches the catalog at connect time and the model expands one on demand
//! with the get_skill tool. Both sources are advertised side by side in the
//! system prompt.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
    /// TASK-331 semantic metadata — coarse topic buckets a skill belongs to
    /// (`infrastructure`, `troubleshooting`, `release`, `perf`, `design`,
    /// `review`, …). Empty when the SKILL.md predates the schema.
    pub categories: Vec<String>,
    /// TASK-331 — repo/project scopes this skill is meant for (`aish`,
    /// `cloudinero`, `all`, …). Empty ⇒ unscoped (treated as broadly applicable).
    pub applies_to: Vec<String>,
    /// TASK-331 — intent patterns this skill should be SUPPRESSED on
    /// (`review`, `design`, `ui`, …), so a match on an unwanted intent can be
    /// filtered out before the per-turn nudge fires.
    pub unwanted_for: Vec<String>,
}

/// Scan a skills directory. Missing dir → no skills; malformed entries are
/// skipped silently (the file is still readable by hand if someone wants it).
pub fn load(dir: &Path) -> Vec<Skill> {
    let mut skills = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return skills;
    };
    for entry in entries.flatten() {
        let skill_md = entry.path().join("SKILL.md");
        let Ok(text) = std::fs::read_to_string(&skill_md) else {
            continue;
        };
        if let Some((name, description)) = parse_frontmatter(&text) {
            let (categories, applies_to, unwanted_for) = parse_semantic_metadata(&text);
            skills.push(Skill {
                name,
                description,
                path: skill_md,
                categories,
                applies_to,
                unwanted_for,
            });
        }
    }
    skills.sort_by(|a, b| a.name.cmp(&b.name));
    skills
}

/// Load the full skill catalog: the installed skills in `skills_dir`
/// (`~/.aish/skills`) PLUS every skill contributed by discovered plugins under
/// the sibling `plugins/` directory (`~/.aish/plugins/<id>/skills/`). Plugins
/// thus **expand the skill registry** without the caller wiring anything up.
///
/// The plugins directory is derived as `skills_dir`'s sibling `plugins/`, so a
/// standard `~/.aish/skills` maps to `~/.aish/plugins`. An installed skill wins
/// on a name collision — a user's `~/.aish/skills/<name>` always shadows a
/// plugin's same-named skill. Tests that want *only* the on-disk skills keep
/// calling [`load`]; production call sites use this.
pub fn load_catalog(skills_dir: &Path) -> Vec<Skill> {
    let mut skills = load(skills_dir);
    if let Some(plugins_dir) = skills_dir.parent().map(|p| p.join("plugins")) {
        let have: std::collections::HashSet<String> =
            skills.iter().map(|s| s.name.clone()).collect();
        for sk in crate::plugins::plugin_skills(&plugins_dir) {
            if !have.contains(&sk.name) {
                skills.push(sk);
            }
        }
        skills.sort_by(|a, b| a.name.cmp(&b.name));
    }
    skills
}

/// Pull `name:` and `description:` out of a `---`-fenced frontmatter block.
/// Single-line values only — that's what the convention uses in practice.
/// Shared with the skill.fish importer, which validates fetched SKILL.md files.
pub fn parse_frontmatter(text: &str) -> Option<(String, String)> {
    let rest = text.strip_prefix("---")?;
    let end = rest.find("\n---")?;
    let mut name = None;
    let mut description = None;
    for line in rest[..end].lines() {
        if let Some(v) = line.strip_prefix("name:") {
            name = Some(v.trim().to_string());
        } else if let Some(v) = line.strip_prefix("description:") {
            description = Some(v.trim().to_string());
        }
    }
    Some((name?, description?))
}

/// Return the inner text of a `---`-fenced frontmatter block (between the
/// opening `---` and the terminating `\n---`), or `None` when absent.
fn frontmatter_block(text: &str) -> Option<&str> {
    let rest = text.strip_prefix("---")?;
    let end = rest.find("\n---")?;
    Some(&rest[..end])
}

/// TASK-331 — pull the three semantic-metadata list fields out of a SKILL.md
/// frontmatter block: `(categories, applies-to, unwanted-for)`. Each is a
/// `Vec<String>` that is empty when the field is absent. Both YAML list shapes
/// are accepted:
///   inline  →  `categories: [infrastructure, troubleshooting]`
///   block   →  `categories:\n  - infrastructure\n  - troubleshooting`
/// Values are trimmed and surrounding quotes stripped; empty entries dropped.
pub fn parse_semantic_metadata(text: &str) -> (Vec<String>, Vec<String>, Vec<String>) {
    let Some(front) = frontmatter_block(text) else {
        return (Vec::new(), Vec::new(), Vec::new());
    };
    (
        parse_list_field(front, "categories"),
        parse_list_field(front, "applies-to"),
        parse_list_field(front, "unwanted-for"),
    )
}

/// Trim whitespace then strip a single pair of matching surrounding quotes.
fn clean_item(s: &str) -> String {
    let t = s.trim();
    let t = t
        .strip_prefix('"')
        .and_then(|r| r.strip_suffix('"'))
        .or_else(|| t.strip_prefix('\'').and_then(|r| r.strip_suffix('\'')))
        .unwrap_or(t);
    t.trim().to_string()
}

/// Extract one YAML list field (`key`) from a frontmatter block, supporting
/// both the inline `[a, b]` form and the multi-line `- a` block form.
fn parse_list_field(front: &str, key: &str) -> Vec<String> {
    let lines: Vec<&str> = front.lines().collect();
    for (i, raw) in lines.iter().enumerate() {
        // Match `key:` only at the top level (no leading indentation), so a
        // nested key never collides with a real field.
        let Some(rest) = raw.strip_prefix(key) else {
            continue;
        };
        let Some(after) = rest.strip_prefix(':') else {
            continue;
        };
        let value = after.trim();
        if value.starts_with('[') {
            // Inline flow sequence: [a, b, c]
            let inner = value.trim_start_matches('[').trim_end_matches(']');
            return inner
                .split(',')
                .map(clean_item)
                .filter(|s| !s.is_empty())
                .collect();
        }
        if !value.is_empty() {
            // Scalar on the same line — treat as a single-item list.
            let item = clean_item(value);
            return if item.is_empty() { Vec::new() } else { vec![item] };
        }
        // Block form: subsequent `  - item` lines until indentation ends.
        let mut out = Vec::new();
        for next in &lines[i + 1..] {
            let trimmed = next.trim_start();
            if let Some(item) = trimmed.strip_prefix("- ") {
                let item = clean_item(item);
                if !item.is_empty() {
                    out.push(item);
                }
            } else if trimmed.is_empty() {
                continue;
            } else {
                break;
            }
        }
        return out;
    }
    Vec::new()
}

/// The system-prompt section advertising available skills from both sources.
pub fn render_prompt_section(skills: &[Skill], mcp_skills: &[crate::mcp::McpSkill]) -> String {
    let mut s = String::new();
    if !skills.is_empty() {
        s.push_str(
            "\n\nSkills — expert playbooks on disk. USING a skill simply means reading its \
SKILL.md and carrying out its steps yourself with your normal tools — there is no separate \
command to \"invoke\" a skill, so never claim you can't run one. When a task matches one, read \
its file with read_file FIRST and follow it (it may reference further files next to it), BEFORE \
attempting the task by hand. If you match a skill but CANNOT load it — the SKILL.md is missing \
or unreadable, or the tool call fails — SURFACE that (debug it or report which skill failed and \
why); do NOT silently fall through to hand-rolling a worse version, because a hidden downgrade to \
a lower tier is more costly than a visible gap. If NO installed skill matches a substantial task, aish may suggest an \
installable one — surface that `:skill add <ref>` recommendation to the user rather than faking or \
hand-rolling the skill:\n",
        );
        for sk in skills {
            s.push_str(&format!(
                "- {} ({}): {}\n",
                sk.name,
                sk.path.display(),
                sk.description
            ));
        }
    }
    if !mcp_skills.is_empty() {
        s.push_str(
            "\n\nMCP skills — the same idea, published by connected MCP servers. When a task \
matches one, call get_skill {server, name, args} FIRST and follow what it returns. If a get_skill call ERRORS, \
surface it — name the skill and the error, then debug or report it — rather than quietly \
substituting a generic, lower-tier approach; the dedicated tool failing is signal, not something to \
paper over:\n",
        );
        for sk in mcp_skills {
            let args = if sk.args.is_empty() {
                String::new()
            } else {
                // `name` required, `name?` optional — compact enough to scan
                let list: Vec<String> = sk
                    .args
                    .iter()
                    .map(|(n, req)| if *req { n.clone() } else { format!("{n}?") })
                    .collect();
                format!(" (args: {})", list.join(", "))
            };
            s.push_str(&format!(
                "- {} (server: {}){}: {}\n",
                sk.name, sk.server, args, sk.description
            ));
        }
    }
    s
}

/// The MCP skills the INTERACTIVE shell is allowed to advertise — the routing /
/// decision skills it uses to decide WHETHER to offload work, not the heavy
/// code-work or agent-dispatch skills that actually do it. The interactive agent
/// is a light-touch router: it keeps these two and hands every heavier task to a
/// background coordinator (which sees the full catalog), so the heavy lifting
/// never runs inline at the prompt.
pub const INTERACTIVE_MCP_SKILLS: &[&str] = &["atum/should-i-hire-an-agent", "atum/pick-model"];

/// Narrow a full MCP skill catalog down to the routing subset the interactive
/// agent may see (`INTERACTIVE_MCP_SKILLS`). A background coordinator passes the
/// catalog through unfiltered; only the interactive shell applies this.
pub fn interactive_mcp_skills(all: &[crate::mcp::McpSkill]) -> Vec<crate::mcp::McpSkill> {
    all.iter()
        .filter(|s| INTERACTIVE_MCP_SKILLS.contains(&s.name.as_str()))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frontmatter_parses() {
        let (n, d) = parse_frontmatter(
            "---\nname: my-skill\ndescription: Does things.\nextra: x\n---\nbody",
        )
        .unwrap();
        assert_eq!(n, "my-skill");
        assert_eq!(d, "Does things.");
        assert!(parse_frontmatter("no frontmatter here").is_none());
        assert!(parse_frontmatter("---\nname: only-name\n---\n").is_none());
    }

    #[test]
    fn prompt_section_merges_both_sources() {
        let local = vec![Skill {
            name: "deploy".into(),
            description: "Ship it.".into(),
            path: PathBuf::from("/s/deploy/SKILL.md"),
            ..Default::default()
        }];
        let mcp = vec![crate::mcp::McpSkill {
            server: "atum".into(),
            name: "atum/sprint-status".into(),
            description: "Summarize the sprint.".into(),
            args: vec![("sprintId".into(), true), ("hours".into(), false)],
        }];
        let s = render_prompt_section(&local, &mcp);
        assert!(s.contains("- deploy (/s/deploy/SKILL.md): Ship it."));
        assert!(s.contains(
            "- atum/sprint-status (server: atum) (args: sprintId, hours?): Summarize the sprint."
        ));
        // either source alone still renders; neither → empty
        assert!(render_prompt_section(&local, &[]).contains("deploy"));
        assert!(render_prompt_section(&[], &mcp).contains("get_skill"));
        assert_eq!(render_prompt_section(&[], &[]), "");
    }

    #[test]
    fn interactive_filter_keeps_only_routing_skills() {
        let mk = |name: &str| crate::mcp::McpSkill {
            server: "atum".into(),
            name: name.into(),
            description: "d".into(),
            args: vec![],
        };
        let all = vec![
            mk("atum/should-i-hire-an-agent"),
            mk("atum/pick-model"),
            mk("atum/review-pr"),
            mk("atum/build-agent"),
            mk("atum/invoke-agent"),
        ];
        let kept: Vec<String> = interactive_mcp_skills(&all)
            .into_iter()
            .map(|s| s.name)
            .collect();
        // Only the two routing skills survive; every heavy code-work / dispatch
        // skill is hidden from the interactive agent.
        assert_eq!(kept, vec!["atum/should-i-hire-an-agent", "atum/pick-model"]);
        // Empty catalog stays empty.
        assert!(interactive_mcp_skills(&[]).is_empty());
    }

    #[test]
    fn load_catalog_merges_plugin_skills() {
        // Build an `~/.aish`-shaped layout: <root>/skills + <root>/plugins.
        let root = std::env::temp_dir().join(format!(
            "aish-catalog-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let skills_dir = root.join("skills");
        let plugin_skill = root.join("plugins").join("hello-world").join("skills").join("hello-world");
        std::fs::create_dir_all(skills_dir.join("deploy")).unwrap();
        std::fs::create_dir_all(&plugin_skill).unwrap();
        std::fs::write(
            skills_dir.join("deploy").join("SKILL.md"),
            "---\nname: deploy\ndescription: Ship it.\n---\nbody",
        )
        .unwrap();
        std::fs::write(
            root.join("plugins").join("hello-world").join("plugin.json"),
            r#"{"id":"hello-world"}"#,
        )
        .unwrap();
        std::fs::write(
            plugin_skill.join("SKILL.md"),
            "---\nname: hello-world\ndescription: Greet.\n---\nbody",
        )
        .unwrap();

        let names: Vec<String> = load_catalog(&skills_dir).into_iter().map(|s| s.name).collect();
        // Installed skill + plugin-contributed skill, sorted by name.
        assert_eq!(names, vec!["deploy", "hello-world"]);

        // `load` alone (no plugin merge) sees only the installed skill.
        assert_eq!(load(&skills_dir).len(), 1);

        let _ = std::fs::remove_dir_all(&root);
    }

    // ---- TASK-331: semantic-metadata parser + validation ----

    #[test]
    fn parse_semantic_metadata_inline_flow_lists() {
        let md = "---\nname: aish_sre\ndescription: SRE playbook.\n\
categories: [infrastructure, troubleshooting, release]\n\
applies-to: [aish]\n\
unwanted-for: [design, review]\n---\nbody";
        let (cats, applies, unwanted) = parse_semantic_metadata(md);
        assert_eq!(cats, vec!["infrastructure", "troubleshooting", "release"]);
        assert_eq!(applies, vec!["aish"]);
        assert_eq!(unwanted, vec!["design", "review"]);
    }

    #[test]
    fn parse_semantic_metadata_block_lists_and_quotes() {
        let md = "---\nname: review\ndescription: Reviews.\n\
categories:\n  - review\n  - \"code-quality\"\n\
applies-to:\n  - 'all'\n\
unwanted-for:\n  - infrastructure\n  - perf\n---\nbody";
        let (cats, applies, unwanted) = parse_semantic_metadata(md);
        assert_eq!(cats, vec!["review", "code-quality"]);
        assert_eq!(applies, vec!["all"]);
        assert_eq!(unwanted, vec!["infrastructure", "perf"]);
    }

    #[test]
    fn parse_semantic_metadata_missing_fields_default_empty() {
        // A pre-schema SKILL.md with no semantic fields yields three empty vecs
        // and must not panic.
        let md = "---\nname: rust-pro\ndescription: Rust.\ncategories: []\n---\nbody";
        let (cats, applies, unwanted) = parse_semantic_metadata(md);
        assert!(cats.is_empty());
        assert!(applies.is_empty());
        assert!(unwanted.is_empty());
        // No frontmatter at all → still empty, no panic.
        let (c2, a2, u2) = parse_semantic_metadata("plain body, no frontmatter");
        assert!(c2.is_empty() && a2.is_empty() && u2.is_empty());
    }

}
