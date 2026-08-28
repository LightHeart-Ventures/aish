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
//! Scope: this module covers BOTH halves of the skill-awareness design.
//!   * "YES, an installed skill matches" → [`hint`] surfaces the local SKILL.md
//!     so the model reads and follows it (the engine prepends it to the turn).
//!   * "NO installed skill matches a substantial task" → [`recommend_install`]
//!     names an installable skill from the registry catalog so the model can
//!     RECOMMEND it (`:skill add <ref>`) instead of faking or hand-rolling it.
//!     To keep the per-turn hot path fast and offline, the recommendation reads
//!     the binary-shipped registry index (`skill_provider::local_index_catalog`)
//!     — NO network round-trip per prompt. A full live search across mcpmarket /
//!     skill.fish stays explicit via `:skill search <query>` / `--skill-search`
//!     (see `skill_provider`).

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

/// Relevance score of a `(name, description)` pair for a tokenized task: for
/// each DISTINCT task token, [`NAME_WEIGHT`] if it matches a token in the name,
/// else 1 if it matches a token in the description, else nothing. A token is
/// counted once, at the higher (name) weight when it matches both. Shared by the
/// installed-skill nudge ([`relevance`]) and the registry recommendation
/// ([`recommend_install`]).
fn relevance_named(task_tokens: &[String], name: &str, description: &str) -> usize {
    let name_toks = tokens(name);
    let desc_toks = tokens(description);
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

/// Relevance score of an installed `skill` for a tokenized task — a thin wrapper
/// over [`relevance_named`] on the skill's name + description.
pub fn relevance(task_tokens: &[String], skill: &Skill) -> usize {
    relevance_named(task_tokens, &skill.name, &skill.description)
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
/// model at each one's SKILL.md. Pure + unit-tested; the engine does the prepend.
pub fn hint(task: &str, skills: &[Skill]) -> Option<String> {
    // Semantic ranking (TASK-332): metadata-aware when a skill carries
    // `categories`/`applies-to`/`unwanted-for` frontmatter, and identical to the
    // lexical baseline for metadata-free skills (base score == keyword
    // relevance, no boosts/suppression). `repo` is None here — production
    // repo-detection for the `applies-to` boost is a follow-up, so that factor
    // stays dormant until then. The `>= MIN_SCORE` gate preserves the
    // pre-TASK-332 quality bar so a single incidental keyword never trips a nudge
    // (`skill_match` itself only filters `score > 0`).
    let ranked = skill_match(task, None, skills);
    let top: Vec<&Scored> = ranked
        .iter()
        .filter(|s| s.score >= MIN_SCORE as i32)
        .take(MAX_HINTS)
        .collect();
    if top.is_empty() {
        return None;
    }
    if top.len() == 1 {
        let m = top[0];
        Some(format!(
            "[aish skill-awareness] Your installed `{}` skill fits this task. USING a skill just \
means reading its SKILL.md and carrying out its steps yourself with your normal tools — there is \
no separate command to \"invoke\" it. Read it FIRST with read_file(\"{}\") and follow it, BEFORE \
attempting the task manually ({}).",
            m.skill.name,
            m.skill.path.display(),
            m.skill.description.trim_end_matches('.'),
        ))
    } else {
        let mut s = String::from(
            "[aish skill-awareness] These installed skills look relevant to this task — \
read the best-fitting one FIRST with read_file and follow it:",
        );
        for m in &top {
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

/// The filename aish treats as a repo's machine-readable spec. When present in
/// the working directory it captures repo conventions (build/test commands,
/// layout, guardrails) the model should honor before touching code — see the
/// "Repo mode: `.repospec.json` FIRST, then code" system-prompt rule.
pub const REPOSPEC_FILE: &str = ".repospec.json";

/// A short reminder to fold in ALONGSIDE a skill-awareness hint when a
/// [`REPOSPEC_FILE`] exists in the working directory. A matched skill's SKILL.md
/// steps are generic; the repo's `.repospec.json` carries the project-specific
/// conventions that should shape HOW those steps are applied here. The engine
/// does the cwd existence check (keeping [`hint`] pure) and appends this note so
/// the model reads the spec first and keeps it in mind while following the skill.
pub fn repospec_reminder() -> String {
    format!(
        "[aish skill-awareness] This repo has a `{REPOSPEC_FILE}` — read it FIRST and keep its \
conventions (build/test commands, layout, guardrails) in mind while applying the skill above; \
let the repo spec win where it conflicts with the skill's generic steps."
    )
}

/// Minimum number of SIGNIFICANT tokens (after stopword/short-word filtering) a
/// task must carry to be "skill-worthy" — substantial enough that recommending
/// an installable skill is worth the interruption. A one- or two-word command
/// (`ls /tmp`, `git status`) falls below the bar; a real task ("resolve the
/// merge conflicts on this branch") clears it.
const SKILL_WORTHY_MIN_TOKENS: usize = 4;

/// Whether `task` is substantial enough to warrant an offline registry
/// recommendation when no installed skill matched. Trivial commands never trip
/// a "you could install …" nudge.
pub fn is_skill_worthy(task: &str) -> bool {
    tokens(task).len() >= SKILL_WORTHY_MIN_TOKENS
}

/// A recommended-but-not-installed skill from the registry catalog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recommendation {
    /// The `owner/name` (or GitHub URL) ref to pass to `:skill add` /
    /// `--skill-fetch`. Doubles as the per-session dedup key.
    pub reference: String,
    /// The pre-rendered `[aish skill-awareness] …` note to fold into the turn.
    pub note: String,
}

/// Recommend an installable skill from `catalog` (a registry index / search
/// result set) for `task`, when NONE of the `installed` skills already fits.
/// Pure + unit-tested: the caller supplies the catalog (loaded offline from the
/// binary-shipped index — see `skill_provider::local_index_catalog`).
///
/// Returns `None` unless the task is skill-worthy AND a catalog entry clears the
/// same name-level relevance bar ([`MIN_SCORE`]) the local nudge uses AND that
/// entry isn't already installed (matched by skill name). The note tells the
/// model to RECOMMEND the install to the user (`:skill add <ref>`) rather than
/// fake, or manually re-implement, a skill that isn't installed.
pub fn recommend_install(
    task: &str,
    installed: &[Skill],
    catalog: &[crate::skill_provider::SearchResult],
) -> Option<Recommendation> {
    if !is_skill_worthy(task) {
        return None;
    }
    let task_tokens = tokens(task);
    if task_tokens.is_empty() {
        return None;
    }
    let installed_names: HashSet<&str> = installed.iter().map(|s| s.name.as_str()).collect();
    // Rank catalog entries by the same name/description relevance the local nudge
    // uses; skip anything already installed and anything below the bar. Highest
    // score wins; ties break by reference so the pick is deterministic.
    let mut best: Option<(&crate::skill_provider::SearchResult, usize)> = None;
    for entry in catalog {
        let name = entry.name.trim();
        if name.is_empty() || installed_names.contains(name) {
            continue;
        }
        let score = relevance_named(&task_tokens, &entry.name, &entry.description);
        if score < MIN_SCORE {
            continue;
        }
        let better = match &best {
            None => true,
            Some((cur, cur_score)) => {
                score > *cur_score
                    || (score == *cur_score && entry.ref_or_synth() < cur.ref_or_synth())
            }
        };
        if better {
            best = Some((entry, score));
        }
    }
    let (entry, _score) = best?;
    let reference = entry.ref_or_synth();
    let desc = entry.description.trim().trim_end_matches('.');
    let suffix = if desc.is_empty() {
        String::new()
    } else {
        format!(" ({desc})")
    };
    let note = format!(
        "[aish skill-awareness] No installed skill fits this task, but the skill registry has \
`{reference}`{suffix} — it looks relevant. RECOMMEND it to the user: they can install it with \
`:skill add {reference}`, after which you'd read its SKILL.md and follow it. Do NOT pretend to \
run, or silently hand-roll, a skill that isn't installed — surface the recommendation instead."
    );
    Some(Recommendation { reference, note })
}

// ---------------------------------------------------------------------------
// TASK-332: semantic skill-matching over the optional frontmatter metadata
// (`categories:`, `applies-to:`, `unwanted-for:` — see `skills::parse_semantic_meta`).
//
// The keyword nudge above ([`hint`]) is purely lexical. This layer adds INTENT:
// a task's wording implies category tags (e.g. "token efficiency" → performance
// + infrastructure), which (a) BOOST skills whose `categories` overlap the
// intent, (b) SUPPRESS skills that list a matched intent in `unwanted-for`, and
// (c) get a repo-scope 2× multiplier when the skill's `applies-to` names the
// current repo. Skills with no metadata simply fall back to the generic keyword
// relevance score, so nothing regresses.
// ---------------------------------------------------------------------------

/// Points added per intent→category overlap. Larger than a description keyword
/// hit (weight 1) and equal to a name hit ([`NAME_WEIGHT`]), so a single
/// declared-category match outweighs incidental prose overlap.
const INTENT_WEIGHT: i32 = 5;

/// Multiplier applied to a skill's score when its `applies-to` names the active
/// repo — an in-repo skill is far more likely to be the right playbook.
const REPO_BOOST: i32 = 2;

/// Cap on semantic matches returned, mirroring [`MAX_HINTS`].
pub const MAX_MATCHES: usize = 2;

/// Keyword → intent-category map. A task token that matches an entry (via the
/// same prefix-aware [`token_matches`] used everywhere else) contributes that
/// category to the task's intent set. Several efficiency/ops words map to BOTH
/// `performance` and `infrastructure` (TASK-332: "token/performance/metrics"
/// signals favor infra + perf skills), so they appear twice on purpose.
const INTENT_KEYWORDS: &[(&str, &str)] = &[
    // performance / efficiency signals (dual-tagged perf + infra)
    ("token", "performance"),
    ("token", "infrastructure"),
    ("performance", "performance"),
    ("performance", "infrastructure"),
    ("metrics", "performance"),
    ("metrics", "infrastructure"),
    // performance-only signals
    ("perf", "performance"),
    ("latency", "performance"),
    ("throughput", "performance"),
    ("optimize", "performance"),
    ("optimization", "performance"),
    ("efficiency", "performance"),
    ("efficient", "performance"),
    ("profiling", "performance"),
    ("benchmark", "performance"),
    ("memory", "performance"),
    // infrastructure / ops signals
    ("infrastructure", "infrastructure"),
    ("deploy", "infrastructure"),
    ("terraform", "infrastructure"),
    ("release", "infrastructure"),
    ("cargo", "infrastructure"),
    // code-review signals
    ("review", "code-review"),
    ("diff", "code-review"),
    ("correctness", "code-review"),
    ("lint", "code-review"),
    ("refactor", "code-review"),
    ("pull", "code-review"),
];

/// Derive the set of intent-category tags a task's wording implies. Empty when
/// the task uses no recognized signal word — callers then fall back to generic
/// keyword relevance.
fn task_intents(task: &str) -> HashSet<String> {
    let toks = tokens(task);
    let mut intents = HashSet::new();
    for t in &toks {
        for (kw, tag) in INTENT_KEYWORDS {
            if token_matches(t, kw) {
                intents.insert((*tag).to_string());
            }
        }
    }
    intents
}

/// One semantically-scored skill with a human-readable explanation of every
/// factor that moved its score — surfaced under `AISH_SKILL_MATCH_DEBUG` so an
/// operator can see WHY each skill ranked where it did.
#[derive(Debug, Clone)]
pub struct Scored<'a> {
    pub skill: &'a Skill,
    pub score: i32,
    pub reasons: Vec<String>,
}

/// Semantic match of `task` (optionally scoped to `repo`) against a skill
/// catalog, using the TASK-332 frontmatter metadata on top of keyword
/// relevance. Returns up to [`MAX_MATCHES`] skills, highest score first (ties
/// broken by name), excluding any skill suppressed by an `unwanted-for`
/// anti-match or scoring zero.
///
/// Scoring per skill:
///   1. `unwanted-for ∩ task-intent` non-empty → hard-suppress (score 0).
///   2. generic keyword relevance → base score (metadata-free fallback).
///   3. `categories ∩ task-intent` → `+INTENT_WEIGHT` each.
///   4. `applies-to` contains `repo` → whole score `× REPO_BOOST`.
pub fn skill_match<'a>(task: &str, repo: Option<&str>, skills: &'a [Skill]) -> Vec<Scored<'a>> {
    let task_tokens = tokens(task);
    let intents = task_intents(task);
    let repo_l = repo.map(|r| r.to_ascii_lowercase());

    let mut scored: Vec<Scored<'a>> = Vec::with_capacity(skills.len());
    for skill in skills {
        let mut reasons = Vec::new();

        // (1) Anti-match: a task intent the skill explicitly opts out of → drop it.
        let anti: Vec<&str> = skill
            .unwanted_for
            .iter()
            .filter(|u| intents.contains(u.as_str()))
            .map(|u| u.as_str())
            .collect();
        if !anti.is_empty() {
            reasons.push(format!("suppressed: task intent {anti:?} in unwanted-for"));
            scored.push(Scored {
                skill,
                score: 0,
                reasons,
            });
            continue;
        }

        // (2) Generic keyword relevance (works for metadata-free skills too).
        let base = relevance(&task_tokens, skill) as i32;
        if base > 0 {
            reasons.push(format!("keyword relevance +{base}"));
        }
        let mut score = base;

        // (3) Intent → declared-category boost.
        let hits: Vec<&str> = skill
            .categories
            .iter()
            .filter(|c| intents.contains(c.as_str()))
            .map(|c| c.as_str())
            .collect();
        if !hits.is_empty() {
            let add = INTENT_WEIGHT * hits.len() as i32;
            score += add;
            reasons.push(format!("intent/category {hits:?} +{add}"));
        }

        // (4) Repo-scope multiplier.
        if let Some(r) = &repo_l {
            if skill.applies_to.iter().any(|a| a == r) {
                score *= REPO_BOOST;
                reasons.push(format!("applies-to '{r}' ×{REPO_BOOST}"));
            }
        }

        scored.push(Scored {
            skill,
            score,
            reasons,
        });
    }

    if skill_match_debug_enabled() {
        for s in &scored {
            eprintln!(
                "[skill-match] {:<28} score={:>3} :: {}",
                s.skill.name,
                s.score,
                if s.reasons.is_empty() {
                    "no signal".to_string()
                } else {
                    s.reasons.join("; ")
                }
            );
        }
    }

    let mut ranked: Vec<Scored<'a>> = scored.into_iter().filter(|s| s.score > 0).collect();
    ranked.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| a.skill.name.cmp(&b.skill.name))
    });
    ranked.truncate(MAX_MATCHES);
    ranked
}

/// Whether `AISH_SKILL_MATCH_DEBUG` requests per-skill scoring diagnostics on
/// stderr (`1`/`true`/`on`).
fn skill_match_debug_enabled() -> bool {
    matches!(
        std::env::var("AISH_SKILL_MATCH_DEBUG").ok().as_deref(),
        Some("1") | Some("true") | Some("on")
    )
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

    /// Build a skill WITH the TASK-332 semantic metadata populated.
    fn skill_meta(
        name: &str,
        description: &str,
        categories: &[&str],
        applies_to: &[&str],
        unwanted_for: &[&str],
    ) -> Skill {
        let lower = |xs: &[&str]| xs.iter().map(|s| s.to_ascii_lowercase()).collect();
        Skill {
            name: name.into(),
            description: description.into(),
            path: PathBuf::from(format!("/skills/{name}/SKILL.md")),
            categories: lower(categories),
            applies_to: lower(applies_to),
            unwanted_for: lower(unwanted_for),
        }
    }

    /// Catalog modeled on the real installed skills, annotated with metadata.
    fn semantic_catalog() -> Vec<Skill> {
        vec![
            skill_meta(
                "aish_sre",
                "SRE playbook for aish: releases, CI, OOM, coordinators.",
                &["infrastructure", "performance"],
                &["aish"],
                &["code-review"],
            ),
            skill_meta(
                "rust-pro",
                "Master Rust with async, the type system, and performance.",
                &["performance", "rust"],
                &["aish"],
                &["code-review"],
            ),
            skill_meta(
                "code-review-excellence",
                "Effective code review, PR feedback, catch bugs early.",
                &["code-review"],
                &[],
                &["performance", "infrastructure"],
            ),
            skill_meta(
                "terraform-aws-modules",
                "Reusable Terraform AWS modules and state management.",
                &["infrastructure"],
                &["cloudinero"],
                &["code-review"],
            ),
        ]
    }

    #[test]
    fn token_efficiency_ranks_infra_perf_and_filters_code_review() {
        let c = semantic_catalog();
        let r = skill_match("implement token efficiency in the shell", Some("aish"), &c);
        let names: Vec<&str> = r.iter().map(|s| s.skill.name.as_str()).collect();
        assert_eq!(names.len(), 2, "got {names:?}");
        assert!(names.contains(&"aish_sre"), "got {names:?}");
        assert!(names.contains(&"rust-pro"), "got {names:?}");
        assert!(!names.contains(&"code-review-excellence"), "got {names:?}");
        // aish_sre has 2 category hits vs rust-pro's 1; equal repo boost → aish_sre first.
        assert_eq!(r[0].skill.name, "aish_sre");
    }

    #[test]
    fn code_review_task_surfaces_review_skill_and_filters_infra() {
        let c = semantic_catalog();
        let r = skill_match("code review the auth module changes", Some("aish"), &c);
        let names: Vec<&str> = r.iter().map(|s| s.skill.name.as_str()).collect();
        assert!(names.contains(&"code-review-excellence"), "got {names:?}");
        assert!(!names.contains(&"aish_sre"), "got {names:?}");
        assert!(!names.contains(&"terraform-aws-modules"), "got {names:?}");
        assert_eq!(r[0].skill.name, "code-review-excellence");
    }

    #[test]
    fn repo_scope_boost_promotes_applies_to_match() {
        let c = vec![
            skill_meta("rust-pro", "Rust performance.", &["performance"], &["aish"], &[]),
            skill_meta(
                "perf-generic",
                "Generic performance tuning.",
                &["performance"],
                &["other"],
                &[],
            ),
        ];
        let r = skill_match("optimize performance", Some("aish"), &c);
        assert_eq!(r[0].skill.name, "rust-pro");
        assert!(r[0].score > r[1].score, "repo-scoped skill should outrank: {r:?}");
    }

    #[test]
    fn anti_match_suppresses_even_on_keyword_overlap() {
        // Description keyword-matches the task, but unwanted-for lists the intent.
        let c = vec![skill_meta(
            "no-perf-here",
            "Handles performance metrics dashboards.",
            &[],
            &[],
            &["performance"],
        )];
        let r = skill_match("improve performance metrics", None, &c);
        assert!(r.is_empty(), "anti-match should suppress: {r:?}");
    }

    #[test]
    fn generic_keyword_fallback_when_no_intent() {
        // No intent keyword present → generic name/description relevance decides.
        let c = vec![
            skill_meta(
                "dependency-audit",
                "Audit dependencies for vulnerabilities and licenses.",
                &[],
                &[],
                &[],
            ),
            skill_meta("rust-pro", "Rust patterns.", &["performance"], &[], &[]),
        ];
        let r = skill_match("audit our dependencies for vulnerabilities", None, &c);
        assert_eq!(r[0].skill.name, "dependency-audit");
    }

    #[test]
    fn returns_at_most_two_matches() {
        let c = semantic_catalog();
        let r = skill_match(
            "optimize token performance and infrastructure metrics",
            Some("aish"),
            &c,
        );
        assert!(r.len() <= MAX_MATCHES, "got {} matches", r.len());
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
    fn repospec_reminder_names_the_spec_file_and_is_a_skill_awareness_note() {
        let r = repospec_reminder();
        assert!(r.starts_with("[aish skill-awareness]"), "{r}");
        assert!(r.contains(REPOSPEC_FILE), "{r}");
        assert_eq!(REPOSPEC_FILE, ".repospec.json");
    }

    // ---- registry recommendation (no installed skill matched) -----------

    fn search_result(name: &str, description: &str, reference: &str) -> crate::skill_provider::SearchResult {
        crate::skill_provider::SearchResult {
            name: name.into(),
            author: "anthropic".into(),
            description: description.into(),
            version: "1.0.0".into(),
            reference: reference.into(),
            stars: 0,
        }
    }

    /// A small registry catalog modeled on the binary-shipped index.json.
    fn registry() -> Vec<crate::skill_provider::SearchResult> {
        vec![
            search_result("git-rebase", "Rebase and squash git commits interactively.", "anthropic/git-rebase"),
            search_result("kubernetes-deploy", "Deploy applications to Kubernetes clusters.", "anthropic/kubernetes-deploy"),
            search_result("terraform-plan", "Plan and apply Terraform infrastructure changes.", "anthropic/terraform-plan"),
        ]
    }

    #[test]
    fn skill_worthy_gate_filters_trivial_tasks() {
        // Substantial multi-word tasks clear the bar; short commands don't.
        assert!(is_skill_worthy("deploy this application to a kubernetes cluster"));
        assert!(!is_skill_worthy("ls /tmp"));
        assert!(!is_skill_worthy("git status"));
        assert!(!is_skill_worthy(""));
    }

    #[test]
    fn recommends_a_relevant_registry_skill_when_none_installed() {
        let rec = recommend_install(
            "deploy the service to our kubernetes cluster",
            &[],
            &registry(),
        )
        .expect("a kubernetes task should match the kubernetes-deploy skill");
        assert_eq!(rec.reference, "anthropic/kubernetes-deploy");
        assert!(rec.note.starts_with("[aish skill-awareness]"), "{}", rec.note);
        assert!(rec.note.contains(":skill add anthropic/kubernetes-deploy"), "{}", rec.note);
        // The note steers away from faking/hand-rolling the skill.
        assert!(rec.note.to_lowercase().contains("not pretend"), "{}", rec.note);
    }

    #[test]
    fn no_recommendation_when_the_skill_is_already_installed() {
        // The matching skill is installed → the local nudge handles it; the
        // registry path must NOT also recommend installing it again.
        let installed = vec![skill(
            "kubernetes-deploy",
            "Deploy applications to Kubernetes clusters.",
        )];
        assert_eq!(
            recommend_install(
                "deploy the service to our kubernetes cluster",
                &installed,
                &registry(),
            ),
            None
        );
    }

    #[test]
    fn no_recommendation_for_trivial_or_unmatched_tasks() {
        // Below the skill-worthy token bar.
        assert_eq!(recommend_install("git status", &[], &registry()), None);
        // Substantial, but nothing in the catalog is relevant.
        assert_eq!(
            recommend_install("what is the capital of france today please", &[], &registry()),
            None
        );
    }

    // ── Regression: coordinator preamble pollution (TASK-XXX) ──────────────
    //
    // The coordinator wraps the user's task in a large boilerplate preamble
    // (PHASE0_GUARD + PHASE_PIPELINE + ...) that ends with "\nTASK:\n<user_task>".
    // The PHASE_PIPELINE text contains "cargo" (from "for aish: `cargo test
    // --no-default-features --locked`"), which maps via INTENT_KEYWORDS to the
    // "infrastructure" category.  A skill whose categories include
    // "infrastructure" (e.g. thirdchair-sre) therefore scored ≥ MIN_SCORE even
    // when the user's actual task had nothing to do with it.
    //
    // The fix lives in engine.rs: `task` is extracted from after the last
    // "\nTASK:\n" marker so the boilerplate never reaches skill_match.  These
    // tests verify that the *matcher* itself is not the source of the false
    // positive — i.e. the SHORT user task doesn't score the infra skill at all,
    // while the FULL preamble (with "cargo") does.
    #[test]
    fn short_task_does_not_match_infra_skill() {
        // Simulate what the user actually asked — no preamble keywords.
        let user_task = "review the interaction we just had - there is a bug in aish \
(LightHeart-Ventures/aish) - find and fix it; open pr";
        let infra_skill = skill_meta(
            "thirdchair-sre",
            "Troubleshoot and operate the Thirdchair / Expy production stack on AWS \
— ECS Fargate (webui + workers), ALB, DynamoDB tenant directory, Google-SSO login, \
logs, and deploy/rollout verification. Use for any \"app is down / login 403 / rollout \
stuck / who's running what / tail the logs\" SRE question.",
            &["infrastructure", "sre", "aws", "troubleshooting"],
            &["thirdchair", "expy"],
            &[],
        );
        let skills = [infra_skill];
        let ranked = skill_match(user_task, None, &skills);
        assert!(
            ranked.is_empty(),
            "thirdchair-sre should NOT match a task about fixing aish code; got: {ranked:?}"
        );
    }

    #[test]
    fn full_coordinator_preamble_does_match_infra_skill() {
        // Confirm that the bug was real: if the preamble leaks into the matcher
        // it DOES produce a false positive (score ≥ MIN_SCORE).  The engine fix
        // ensures this string never reaches skill_match in production, but we
        // document it here so the regression is visible.
        let infra_skill = skill_meta(
            "thirdchair-sre",
            "Troubleshoot and operate the Thirdchair / Expy production stack on AWS \
— ECS Fargate (webui + workers), ALB, DynamoDB tenant directory, Google-SSO login, \
logs, and deploy/rollout verification. Use for any \"app is down / login 403 / rollout \
stuck / who's running what / tail the logs\" SRE question.",
            &["infrastructure", "sre", "aws", "troubleshooting"],
            &["thirdchair", "expy"],
            &[],
        );
        // A coordinator preamble fragment containing the "cargo" keyword that
        // triggers the infrastructure intent (via INTENT_KEYWORDS).
        let preamble_fragment = "--- PHASE 4: VALIDATION --- Run the canonical gate once \
(for aish: `cargo test --no-default-features --locked`) and confirm green, then open/finish the PR.\
\nTASK:\nreview the interaction we just had - there is a bug in aish - find and fix it; open pr";
        let skills = [infra_skill];
        let ranked = skill_match(preamble_fragment, None, &skills);
        // The preamble's "cargo" → "infrastructure" intent boosts thirdchair-sre
        // above MIN_SCORE — exactly the false positive the engine fix prevents.
        assert!(
            !ranked.is_empty(),
            "expected the preamble fragment to produce the false-positive score (to document the bug)"
        );
        assert!(
            ranked[0].score >= MIN_SCORE as i32,
            "expected score ≥ MIN_SCORE; got {}",
            ranked[0].score
        );
    }
}
