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

pub struct Skill {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
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
            skills.push(Skill { name, description, path: skill_md });
        }
    }
    skills.sort_by(|a, b| a.name.cmp(&b.name));
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

/// The system-prompt section advertising available skills from both sources.
pub fn render_prompt_section(skills: &[Skill], mcp_skills: &[crate::mcp::McpSkill]) -> String {
    let mut s = String::new();
    if !skills.is_empty() {
        s.push_str(
            "\n\nSkills — expert playbooks on disk. When a task matches one, read its file \
with read_file FIRST and follow it (it may reference further files next to it):\n",
        );
        for sk in skills {
            s.push_str(&format!("- {} ({}): {}\n", sk.name, sk.path.display(), sk.description));
        }
    }
    if !mcp_skills.is_empty() {
        s.push_str(
            "\n\nMCP skills — the same idea, published by connected MCP servers. When a task \
matches one, call get_skill {server, name, args} FIRST and follow what it returns:\n",
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
            s.push_str(&format!("- {} (server: {}){}: {}\n", sk.name, sk.server, args, sk.description));
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
        let (n, d) =
            parse_frontmatter("---\nname: my-skill\ndescription: Does things.\nextra: x\n---\nbody")
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
        }];
        let mcp = vec![crate::mcp::McpSkill {
            server: "atum".into(),
            name: "atum/sprint-status".into(),
            description: "Summarize the sprint.".into(),
            args: vec![("sprintId".into(), true), ("hours".into(), false)],
        }];
        let s = render_prompt_section(&local, &mcp);
        assert!(s.contains("- deploy (/s/deploy/SKILL.md): Ship it."));
        assert!(s.contains("- atum/sprint-status (server: atum) (args: sprintId, hours?): Summarize the sprint."));
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
        let kept: Vec<String> =
            interactive_mcp_skills(&all).into_iter().map(|s| s.name).collect();
        // Only the two routing skills survive; every heavy code-work / dispatch
        // skill is hidden from the interactive agent.
        assert_eq!(kept, vec!["atum/should-i-hire-an-agent", "atum/pick-model"]);
        // Empty catalog stays empty.
        assert!(interactive_mcp_skills(&[]).is_empty());
    }
}
