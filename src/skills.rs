//! Skills — reusable instruction packs in ~/.aish/skills/<name>/SKILL.md.
//!
//! The format is the Claude-skill convention: YAML frontmatter with `name:`
//! and `description:`, then a markdown body. aish lists every skill's name,
//! description, and path in the system prompt; the model reads the SKILL.md
//! (and anything it references) with read_file when a task matches.

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
fn parse_frontmatter(text: &str) -> Option<(String, String)> {
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

/// The system-prompt section advertising available skills.
pub fn render_prompt_section(skills: &[Skill]) -> String {
    if skills.is_empty() {
        return String::new();
    }
    let mut s = String::from(
        "\n\nSkills — expert playbooks on disk. When a task matches one, read its file \
with read_file FIRST and follow it (it may reference further files next to it):\n",
    );
    for sk in skills {
        s.push_str(&format!("- {} ({}): {}\n", sk.name, sk.path.display(), sk.description));
    }
    s
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
}
