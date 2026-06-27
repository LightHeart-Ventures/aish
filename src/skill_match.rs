//! Skill-awareness — per-turn relevance matching of the user's task against the
//! INSTALLED local skill catalog (`~/.aish/skills/<name>/SKILL.md`).
//!
//! The installed skills are already advertised, once, in the system prompt (see
//! `skills::render_prompt_section`), but that static list grows with every skill
//! the user adds and a relevant one is easy to overlook on a given task. This
//! module closes that gap with a light, per-turn nudge: it scores the user's
//! input against the catalog by keyword overlap and, when a skill clearly fits,
//! prepends a short `[aish skill-awareness] …` note to that turn's input
//! pointing the model at the matching SKILL.md.
//!
//! The note is injected into the *turn input* (alongside `engine::seed_context`),
//! NOT into the cached system prompt — so the prompt-cache prefix stays
//! byte-stable and the hint is contextual to the task at hand. It's a hint, not
//! a command: the model still decides whether to read and follow the playbook.
//!
//! Scope: this is the "YES, an installed skill matches" half of the
//! skill-awareness design. The "NO match → query the registry" half is
//! deliberately NOT done on the per-turn hot path — a network round-trip on
//! every prompt would add latency and hit registry rate limits/bot challenges.
//! Discovery stays explicit via `:skill search <query>` / `--skill-search`
//! (see `skill_provider`).

use crate::skills::Skill;
use std::collections::HashSet;

/// A name-token match is worth this much more than a description-token match: a
/// hit on the skill's NAME (e.g. "reviewer", "incident", "profiler") is a much
/// stronger signal of relevance than a hit somewhere in its prose description.
const NAME_WEIGHT: usize = 3;

/// Minimum relevance score for a skill to be surfaced as a hint. Equal to
/// [`NAME_WEIGHT`], so a SINGLE name-token match qualifies on its own, while a
/// description-only match needs three overlapping significant words — a high
/// enough bar that an incidental word or two never trips a false nudge.
const MIN_SCORE: usize = NAME_WEIGHT;

/// At most this many skills are named in one hint, so the note stays a glance,
/// not a wall. The highest-scoring matches win (see [`rank`]).
const MAX_HINTS: usize = 2;

/// Common English / shell filler dropped before scoring — matching one of these
/// must never contribute relevance. Kept small and high-frequency; the `len < 3`
/// floor in [`tokens`] already removes most noise (`a`, `to`, `of`, `is`, …).
const STOPWORDS: &[&str] = &[
    "the", "and", "for", "you", "your", "are", "was", "were", "this", "that", "with", "from",
    "into", "out", "can", "could", "would", "should", "will", "what", "when", "where", "why",
    "how", "who", "please", "help", "need", "want", "use", "using", "get", "got", "let", "any",
    "all", "now", "new", "see", "show", "tell", "make", "made", "did", "does", "done", "run",
    "running", "ran", "have", "has", "had", "about", "some", "they", "them", "then", "than",
    "here", "there", "been", "being", "our", "their", "its",
];

/// Tokenize free text into lowercased significant words: split on every
/// non-alphanumeric boundary (so `pr-reviewer`, `P99`, `incident_response` all
/// split cleanly), drop tokens shorter than three chars, and drop stopwords.
fn tokens(s: &str) -> Vec<String> {
    s.split(|c: char| !c.is_ascii_alphanumeric())
        .map(|w| w.to_ascii_lowercase())
        .filter(|w| w.len() >= 3 && !STOPWORDS.contains(&w.as_str()))
        .collect()
}

/// Whether two significant tokens are "the same word" for matching purposes:
/// exactly equal, or one is a prefix of the other with the shorter at least four
/// chars long. The prefix rule is a cheap stand-in for stemming — it lets
/// `review` match `reviewer`, `deploy` match `deployment`, `test` match
/// `testing` — without pulling in a stemmer dependency. The four-char floor
/// keeps short, generic stems from over-matching.
fn token_matches(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    let (short, long) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    short.len() >= 4 && long.starts_with(short)
}

/// Relevance score of `skill` for a tokenized task: for each DISTINCT task
/// token, [`NAME_WEIGHT`] if it matches a token in the skill's name, else 1 if
/// it matches a token in the description, else nothing. A token is counted once,
/// at the higher (name) weight when it matches both.
pub fn relevance(task_tokens: &[String], skill: &Skill) -> usize {
    let name_toks = tokens(&skill.name);
    let desc_toks = tokens(&skill.description);
    let mut seen: HashSet<&str> = HashSet::new();
    let mut score = 0;
    for t in task_tokens {
        if !seen.insert(t.as_str()) {
            continue; // a repeated task word contributes only once
        }
        if name_toks.iter().any(|n| token_matches(t, n)) {
            score += NAME_WEIGHT;
        } else if desc_toks.iter().any(|d| token_matches(t, d)) {
            score += 1;
        }
    }
    score
}

/// One ranked match: the skill and its relevance score (always `>= MIN_SCORE`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Match<'a> {
    pub skill: &'a Skill,
    pub score: usize,
}

/// Rank the installed skills by relevance to `task`, keeping only those at or
/// above [`MIN_SCORE`], highest score first. Ties break by skill name so the
/// order is stable (and so a tie renders deterministically in the hint).
pub fn rank<'a>(task: &str, skills: &'a [Skill]) -> Vec<Match<'a>> {
    let task_tokens = tokens(task);
    if task_tokens.is_empty() {
        return Vec::new();
    }
    let mut matches: Vec<Match<'a>> = skills
        .iter()
        .map(|skill| Match {
            skill,
            score: relevance(&task_tokens, skill),
        })
        .filter(|m| m.score >= MIN_SCORE)
        .collect();
    matches.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.skill.name.cmp(&b.skill.name)));
    matches
}

/// The per-turn skill-awareness note for `task`, or `None` when no installed
/// skill clears the bar. Names up to [`MAX_HINTS`] top matches and points the
/// model at each one's SKILL.md. Pure + unit-tested; `apply` does the prepend.
pub fn hint(task: &str, skills: &[Skill]) -> Option<String> {
    let matches = rank(task, skills);
    if matches.is_empty() {
        return None;
    }
    let top = &matches[..matches.len().min(MAX_HINTS)];
    if top.len() == 1 {
        let m = &top[0];
        Some(format!(
            "[aish skill-awareness] Your installed `{}` skill looks relevant to this task — \
read its playbook FIRST with read_file(\"{}\") and follow it ({}).",
            m.skill.name,
            m.skill.path.display(),
            m.skill.description.trim_end_matches('.'),
        ))
    } else {
        let mut s = String::from(
            "[aish skill-awareness] These installed skills look relevant to this task — \
read the best-fitting one FIRST with read_file and follow it:",
        );
        for m in top {
            s.push_str(&format!(
                "\n- `{}` ({}): {}",
                m.skill.name,
                m.skill.path.display(),
                m.skill.description,
            ));
        }
        Some(s)
    }
}

/// Prepend the skill-awareness note (when one applies) to a turn's input. The
/// hint is matched on the ORIGINAL `task` text — pass the user's request before
/// any other context-seeding so a prepended preamble can't skew the keyword
/// match. A no-op (returns `input` unchanged) when nothing clears the bar.
pub fn apply(task: &str, input: String, skills: &[Skill]) -> String {
    match hint(task, skills) {
        Some(note) => format!("{note}\n\n{input}"),
        None => input,
    }
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
        }
    }

    /// A small catalog modeled on the real installed skills.
    fn catalog() -> Vec<Skill> {
        vec![
            skill(
                "pr-reviewer",
                "Review a pull request or diff against engineering best practices — correctness, security, tests, style, and risk.",
            ),
            skill(
                "incident-responder",
                "Triage and mitigate incidents — stuck tasks, failed agent runs, error spikes, broken deploys.",
            ),
            skill(
                "performance-profiler",
                "Identify performance bottlenecks from logs, traces, and profiles — slow endpoints, hot paths, P99 latency, N+1 queries.",
            ),
            skill(
                "dependency-audit",
                "Audit project dependencies for known vulnerabilities, outdated versions, and license compliance.",
            ),
        ]
    }

    #[test]
    fn tokens_split_filter_and_lowercase() {
        let t = tokens("Review the PR-diff for P99 latency!");
        // hyphen/space/punct all split; <3-char and stopwords dropped; lowercased.
        assert!(t.contains(&"review".to_string()));
        assert!(t.contains(&"diff".to_string()));
        assert!(t.contains(&"p99".to_string()));
        assert!(t.contains(&"latency".to_string()));
        assert!(!t.contains(&"the".to_string())); // stopword
        assert!(!t.contains(&"pr".to_string())); // < 3 chars
        assert!(!t.contains(&"for".to_string())); // stopword
    }

    #[test]
    fn token_matches_handles_prefix_stems() {
        assert!(token_matches("review", "review"));
        assert!(token_matches("review", "reviewer")); // prefix stem
        assert!(token_matches("deploy", "deployment"));
        assert!(token_matches("testing", "test"));
        // Too short a shared stem doesn't match.
        assert!(!token_matches("cat", "category"));
        // Unrelated words don't match.
        assert!(!token_matches("review", "deploy"));
    }

    #[test]
    fn name_match_alone_qualifies() {
        let skills = catalog();
        // "reviewer" is a name token → single match, weight >= MIN_SCORE.
        let h = hint("can you act as a reviewer on my change", &skills).unwrap();
        assert!(h.contains("pr-reviewer"), "{h}");
        assert!(h.contains("/skills/pr-reviewer/SKILL.md"), "{h}");
        assert!(h.starts_with("[aish skill-awareness]"), "{h}");
    }

    #[test]
    fn description_keywords_can_qualify_without_name() {
        // None of these words are in a NAME, but three hit the pr-reviewer
        // description (correctness, security, style) → score 3 == MIN_SCORE.
        let skills = catalog();
        let h = hint("check this for correctness, security, and style", &skills);
        let h = h.expect("three description hits should qualify");
        assert!(h.contains("pr-reviewer"), "{h}");
    }

    #[test]
    fn weak_overlap_does_not_trigger() {
        // A single incidental description word (one point) is below MIN_SCORE.
        let skills = catalog();
        assert_eq!(hint("update the project license header", &skills), None);
    }

    #[test]
    fn plain_prose_with_no_skill_returns_none() {
        let skills = catalog();
        assert_eq!(hint("what is the capital of texas", &skills), None);
        assert_eq!(hint("list the files in this directory", &skills), None);
    }

    #[test]
    fn empty_catalog_or_empty_task_is_none() {
        assert_eq!(hint("review this PR for security", &[]), None);
        assert_eq!(hint("", &catalog()), None);
        assert_eq!(hint("   ", &catalog()), None);
    }

    #[test]
    fn ranking_orders_by_score_then_name() {
        let skills = catalog();
        // "performance bottleneck latency" hits the profiler name + description
        // strongly; nothing else should rank.
        let ranked = rank("find the performance bottleneck causing P99 latency", &skills);
        assert_eq!(ranked[0].skill.name, "performance-profiler");
        assert!(ranked[0].score >= NAME_WEIGHT);
    }

    #[test]
    fn multiple_matches_render_a_list_capped_at_two() {
        // Craft a task that hits two skills by name: "review" (pr-reviewer) and
        // "incident" (incident-responder).
        let skills = catalog();
        let h = hint("review the incident from the failed deploy", &skills).unwrap();
        assert!(h.contains("pr-reviewer"), "{h}");
        assert!(h.contains("incident-responder"), "{h}");
        // Rendered as a bullet list, not the single-skill sentence.
        assert!(h.contains("\n- `"), "{h}");
        // Never more than MAX_HINTS bullets.
        assert!(h.matches("\n- `").count() <= MAX_HINTS, "{h}");
    }

    #[test]
    fn apply_prepends_note_then_blank_line_then_input() {
        let skills = catalog();
        let out = apply(
            "review this PR for security",
            "review this PR for security".into(),
            &skills,
        );
        assert!(out.starts_with("[aish skill-awareness]"), "{out}");
        assert!(out.contains("\n\nreview this PR for security"), "{out}");
    }

    #[test]
    fn apply_is_a_noop_without_a_match() {
        let skills = catalog();
        let input = "ls -la /tmp".to_string();
        assert_eq!(apply("ls -la /tmp", input.clone(), &skills), input);
    }
}
