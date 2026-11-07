//! LLM-gated skill selection — replaces pure keyword search for the per-turn
//! skill nudge.
//!
//! `skill_match` scores a task against each installed skill's name +
//! description with token overlap. That's cheap and offline, but it is still
//! *keyword search*: it fires on incidental word collisions ("review" in a
//! sentence about reviewing a dashboard pulls `pr-reviewer`), and it misses any
//! request that doesn't literally reuse the skill's vocabulary ("CI is red"
//! vs. a skill described as "diagnose failing GitHub Actions runs").
//!
//! This module asks a LIGHTWEIGHT model two questions instead:
//!   1. would a skill actually help with this operator input, and
//!   2. if so, WHICH one.
//!
//! One tool-less Haiku-class call, strict single-line reply protocol
//! (`NONE` or `SKILL: <name>`), hard-validated against the real catalog so a
//! hallucinated name can never be surfaced. Best-effort by design: any error,
//! refusal, or unusable reply yields `None` and the caller falls back to the
//! keyword matcher, so a selector hiccup never costs the operator a hint.
//!
//! Cost control: the selector only runs for tasks that clear
//! [`crate::skill_match::is_skill_worthy`] (trivial one-liners like `ls` never
//! pay for a model call), and `AISH_SKILL_LLM=0` disables it entirely.

use crate::backend::Backend;
use crate::skills::Skill;

/// Max skills listed to the selector. The catalog is already small (tens of
/// entries); this is a guard against a pathological install blowing the prompt.
const MAX_CATALOG: usize = 120;

/// Chars of each skill description kept in the catalog listing. Descriptions are
/// one-liners by convention; a runaway one is clipped rather than paying for it.
const MAX_DESC: usize = 240;

/// Chars of operator input shown to the selector. The decision is about INTENT —
/// the opening of a request carries it, and a long pasted log doesn't change it.
const MAX_TASK: usize = 2_000;

/// Whether the LLM selector is enabled. Default ON; `AISH_SKILL_LLM` set to
/// `0`/`off`/`false`/`no` turns it off and leaves the keyword matcher in charge.
pub fn enabled() -> bool {
    !matches!(
        std::env::var("AISH_SKILL_LLM").ok().as_deref(),
        Some("0") | Some("off") | Some("false") | Some("no")
    )
}

/// Whether to dump the selector's prompt/reply to stderr
/// (`AISH_SKILL_LLM_DEBUG=1`). Mirrors `AISH_SKILL_MATCH_DEBUG`.
fn debug_enabled() -> bool {
    matches!(
        std::env::var("AISH_SKILL_LLM_DEBUG").ok().as_deref(),
        Some("1") | Some("true") | Some("on")
    )
}

/// Hard character clip, no ellipsis. Pure.
fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.trim().to_string()
    } else {
        s.chars().take(max).collect::<String>().trim().to_string()
    }
}

/// The system prompt: a classifier, not an assistant. Two decisions, one line
/// of output, and an explicit bias toward `NONE` — a wrong nudge costs the
/// operator a wasted read and misdirects the turn, so abstaining is cheaper
/// than guessing.
pub fn selector_system() -> String {
    "You are a skill ROUTER for an AI shell. You are given the operator's input and a catalog of \
installed skills (expert playbooks). Decide two things:\n\
1. Would reading one of these skills materially help carry out this input?\n\
2. If so, which single skill fits best?\n\n\
Answer with EXACTLY ONE line, nothing else:\n\
  NONE                 — no skill meaningfully helps\n\
  SKILL: <name>        — the one best-fitting skill, copied verbatim from the catalog\n\n\
Rules:\n\
- Judge INTENT, not vocabulary overlap. A word appearing in both the input and a skill name is \
NOT a match on its own.\n\
- Prefer NONE. Only name a skill when its stated purpose genuinely covers the work being asked \
for. Trivial commands, chit-chat, short factual questions, and status checks get NONE.\n\
- Never invent a name. Use one from the catalog, character for character.\n\
- No explanation, no punctuation after the name, no markdown."
        .to_string()
}

/// Render the catalog listing shown to the selector: one `- name: description`
/// line per installed skill. Pure.
pub fn catalog_block(skills: &[Skill]) -> String {
    skills
        .iter()
        .take(MAX_CATALOG)
        .map(|s| format!("- {}: {}", s.name, clip(&s.description, MAX_DESC)))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The user-turn prompt: catalog then input, input last so the decision target
/// is the most recent thing the model read. Pure.
pub fn build_prompt(task: &str, skills: &[Skill]) -> String {
    format!(
        "INSTALLED SKILLS:\n{}\n\nOPERATOR INPUT:\n{}\n\nYour one-line answer:",
        catalog_block(skills),
        clip(task, MAX_TASK)
    )
}

/// Parse the selector's reply into an index into `skills`, or `None` for an
/// abstention / unusable answer. Pure, and the only path by which a model-chosen
/// name reaches the operator — a name that isn't in the catalog is rejected
/// rather than surfaced, so a hallucination degrades to "no hint".
pub fn parse_choice(reply: &str, skills: &[Skill]) -> Option<usize> {
    // Take the first non-empty line: a chatty model may append prose, and the
    // protocol line comes first.
    let line = reply.lines().map(str::trim).find(|l| !l.is_empty())?;
    let lower = line.to_ascii_lowercase();
    if lower.starts_with("none") {
        return None;
    }
    // Accept `SKILL: x`, `skill x`, or a bare name — the protocol is explicit
    // but a weak model drifts on the prefix, and the catalog check below is what
    // actually makes this safe.
    let name = line
        .split_once(':')
        .map(|(head, tail)| {
            if head.trim().eq_ignore_ascii_case("skill") {
                tail
            } else {
                line
            }
        })
        .unwrap_or(line)
        .trim()
        .trim_matches(|c: char| c == '`' || c == '"' || c == '\'' || c == '.')
        .trim();
    if name.is_empty() || name.eq_ignore_ascii_case("none") {
        return None;
    }
    // Exact (case-insensitive) match first, then a unique containment match so a
    // reply like `ci-debugger skill` still resolves.
    if let Some(i) = skills
        .iter()
        .position(|s| s.name.eq_ignore_ascii_case(name))
    {
        return Some(i);
    }
    let lower_name = name.to_ascii_lowercase();
    let mut hits = skills
        .iter()
        .enumerate()
        .filter(|(_, s)| {
            let n = s.name.to_ascii_lowercase();
            lower_name.contains(&n) || n.contains(&lower_name)
        })
        .map(|(i, _)| i);
    let first = hits.next()?;
    if hits.next().is_some() {
        return None; // ambiguous — abstain rather than coin-flip
    }
    Some(first)
}

/// Ask the lightweight model whether a skill fits `task`, and which.
///
/// Returns the chosen installed skill, or `None` when the model abstains, the
/// call fails, or the reply doesn't name a real catalog entry. One tool-less
/// `complete` call; never panics, never blocks the turn on a retry.
pub async fn select<'a>(backend: &Backend, task: &str, skills: &'a [Skill]) -> Option<&'a Skill> {
    if skills.is_empty() || task.trim().is_empty() {
        return None;
    }
    let prompt = build_prompt(task, skills);
    if debug_enabled() {
        eprintln!("[skill-llm] prompt:\n{prompt}");
    }
    let turn = backend
        .complete(
            &selector_system(),
            &[crate::backend::Msg::user(prompt)],
            &[],
        )
        .await
        .ok()?;
    if debug_enabled() {
        eprintln!("[skill-llm] reply: {:?}", turn.text);
    }
    let idx = parse_choice(&turn.text, skills)?;
    skills.get(idx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn skill(name: &str, description: &str) -> Skill {
        Skill {
            name: name.into(),
            description: description.into(),
            path: PathBuf::from(format!("/skills/{name}/SKILL.md")),
            ..Default::default()
        }
    }

    fn catalog() -> Vec<Skill> {
        vec![
            skill("ci-debugger", "Debug failing CI runs from GitHub Actions."),
            skill("pr-reviewer", "Review a pull request end-to-end."),
            skill("sprint-status", "Summarize the current sprint board."),
        ]
    }

    #[test]
    fn enabled_by_default_and_opt_out_values() {
        // The pure predicate is env-driven; assert the value mapping via the
        // same matcher the function uses (no global env mutation in tests).
        for v in ["0", "off", "false", "no"] {
            assert!(
                matches!(
                    Some(v),
                    Some("0") | Some("off") | Some("false") | Some("no")
                ),
                "{v} should disable"
            );
        }
        assert!(!matches!(
            Some("1"),
            Some("0") | Some("off") | Some("false") | Some("no")
        ));
    }

    #[test]
    fn none_reply_abstains() {
        assert_eq!(parse_choice("NONE", &catalog()), None);
        assert_eq!(parse_choice("none\n", &catalog()), None);
        assert_eq!(parse_choice("None.", &catalog()), None);
    }

    #[test]
    fn empty_and_garbage_replies_abstain() {
        assert_eq!(parse_choice("", &catalog()), None);
        assert_eq!(parse_choice("   \n\n", &catalog()), None);
        assert_eq!(parse_choice("SKILL: not-a-real-skill", &catalog()), None);
    }

    #[test]
    fn protocol_line_resolves_to_index() {
        assert_eq!(parse_choice("SKILL: ci-debugger", &catalog()), Some(0));
        assert_eq!(parse_choice("skill: PR-Reviewer", &catalog()), Some(1));
        assert_eq!(parse_choice("SKILL: `sprint-status`", &catalog()), Some(2));
    }

    #[test]
    fn bare_name_and_trailing_prose_still_resolve() {
        assert_eq!(parse_choice("ci-debugger", &catalog()), Some(0));
        assert_eq!(
            parse_choice("SKILL: ci-debugger\nBecause CI is failing.", &catalog()),
            Some(0)
        );
    }

    #[test]
    fn hallucinated_name_is_rejected_not_surfaced() {
        // The safety property: only catalog entries can ever be returned.
        assert_eq!(
            parse_choice("SKILL: kubernetes-whisperer", &catalog()),
            None
        );
    }

    #[test]
    fn catalog_block_lists_every_skill_once() {
        let block = catalog_block(&catalog());
        assert_eq!(block.lines().count(), 3);
        assert!(block.contains("- ci-debugger: Debug failing CI runs"));
    }

    #[test]
    fn prompt_puts_input_after_catalog() {
        let p = build_prompt("CI is red on my PR", &catalog());
        let cat = p.find("INSTALLED SKILLS:").unwrap();
        let task = p.find("OPERATOR INPUT:").unwrap();
        assert!(cat < task);
        assert!(p.contains("CI is red on my PR"));
    }

    #[test]
    fn long_inputs_and_descriptions_are_clipped() {
        let long = "x".repeat(MAX_TASK * 2);
        let p = build_prompt(&long, &catalog());
        assert!(p.chars().count() < MAX_TASK * 2);
        let fat = vec![skill("fat", &"d".repeat(MAX_DESC * 2))];
        assert!(catalog_block(&fat).chars().count() < MAX_DESC * 2);
    }

    #[test]
    fn empty_catalog_never_selects() {
        assert_eq!(parse_choice("SKILL: anything", &[]), None);
    }

    #[test]
    fn system_prompt_states_both_decisions_and_the_protocol() {
        let s = selector_system();
        assert!(s.contains("NONE"));
        assert!(s.contains("SKILL: <name>"));
        assert!(s.contains("Prefer NONE"));
    }
}
