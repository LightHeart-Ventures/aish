//! Advisor agent for serial-chain-yield evaluation.
//!
//! When a coordinator hits `SerialChainYield` (12+ consecutive single-call rounds),
//! it optionally invokes an Advisor to classify the pattern:
//!   * **Batching opportunity**: the model is making progress but hasn't batched
//!     independent calls yet. Advise Resume with a batching nudge.
//!   * **Stuck pattern**: the model is repeating the same action in a shallow way.
//!     Escalate to the operator with context.
//!
//! The advisor reads the recent turn-audit journal and synthesizes a lightweight
//! classification without a full LLM invocation (pure heuristics) or delegates to
//! an MCP advisor tool if MCP is available (optional).

use std::collections::HashMap;

/// Classification of a serial-chain-yield pattern.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum YieldClassification {
    /// The model is making distinct progress (different tools/files each round).
    /// Advise Resume with a batching directive.
    BatchingOpportunity,
    /// The model is stuck repeating the same action without meaningful variation.
    /// Escalate to the operator.
    StuckPattern,
    /// Indeterminate — default to Resume with a generic directive.
    Unknown,
}

/// Advice returned by the advisor for a serial-chain-yield.
#[derive(Debug, Clone)]
pub struct YieldAdvice {
    /// The classification of the pattern.
    pub classification: YieldClassification,
    /// A human-readable summary of why (motivation for the classification).
    pub summary: String,
    /// Optional directive to fold into the next round (for Resume classifications).
    pub resume_directive: Option<String>,
}

/// Lightweight advisor that classifies a serial-chain-yield based on heuristics.
/// Reads the turn-audit journal (if available) and detects:
///   * **Batching opportunity**: high diversity of tool names / files across rounds.
///   * **Stuck pattern**: low diversity (repeating same tool/file/parameter).
pub struct SerialYieldAdvisor;

impl SerialYieldAdvisor {
    /// Evaluate a serial-chain-yield pattern given the turn-audit journal
    /// (if available). Falls back to `Unknown` if audit is missing.
    ///
    /// # Arguments
    /// - `turns`: a list of (round_number, tool_names, file_paths) summaries from
    ///   the turn-audit journal (the N most recent rounds).
    pub fn evaluate(turns: &[(usize, Vec<String>, Vec<String>)]) -> YieldAdvice {
        if turns.is_empty() {
            return YieldAdvice {
                classification: YieldClassification::Unknown,
                summary: "No turn audit available for evaluation.".to_string(),
                resume_directive: Some(
                    "[advisor: unknown pattern] Re-planning toward batching: group independent \
calls (greps, reads, edits, runs) into a single round rather than chaining them one-per-round."
                        .to_string(),
                ),
            };
        }

        // Count unique tool names and file paths across the last N turns.
        let mut tool_counts: HashMap<String, usize> = HashMap::new();
        let mut file_counts: HashMap<String, usize> = HashMap::new();

        for (_round, tools, files) in turns {
            for tool in tools {
                *tool_counts.entry(tool.clone()).or_insert(0) += 1;
            }
            for file in files {
                *file_counts.entry(file.clone()).or_insert(0) += 1;
            }
        }

        let total_turns = turns.len();
        let unique_tools = tool_counts.len();
        let unique_files = file_counts.len();
        let max_tool_repeats = tool_counts.values().max().copied().unwrap_or(0);
        let max_file_repeats = file_counts.values().max().copied().unwrap_or(0);

        // Heuristic: if the most-repeated tool/file appears in >80% of rounds,
        // it's a stuck pattern. Otherwise, it's a batching opportunity.
        let tool_repetition_ratio = if total_turns > 0 {
            max_tool_repeats as f64 / total_turns as f64
        } else {
            0.0
        };
        let file_repetition_ratio = if total_turns > 0 {
            max_file_repeats as f64 / total_turns as f64
        } else {
            0.0
        };

        let (classification, summary, resume_directive) = if tool_repetition_ratio > 0.8
            || file_repetition_ratio > 0.8
        {
            let repeated_tool = tool_counts
                .iter()
                .max_by_key(|&(_, &count)| count)
                .map(|(name, _)| name.as_str())
                .unwrap_or("(unknown)");
            let repeated_file = file_counts
                .iter()
                .max_by_key(|&(_, &count)| count)
                .map(|(name, _)| name.as_str())
                .unwrap_or("(unknown)");

            let summary = format!(
                "Stuck pattern detected: {} of the last {} rounds repeated the same action ({} \
on {}). Escalating to operator.",
                max_tool_repeats, total_turns, repeated_tool, repeated_file
            );
            (
                YieldClassification::StuckPattern,
                summary,
                None,
            )
        } else {
            let summary = format!(
                "Batching opportunity: {} rounds used {} unique tools on {} unique files. \
Re-plan to batch independent calls.",
                total_turns, unique_tools, unique_files
            );
            let directive = format!(
                "[advisor: batching opportunity] Your last {} rounds were single-call iterations \
on distinct files/tools ({}% unique tools, {}% unique files). Group independent calls \
(e.g., greps, reads, edits) into a single round instead of chaining them. This reduces \
round-trip latency and spreads load across the rate-limit window.",
                total_turns,
                (unique_tools as f64 / total_turns as f64 * 100.0) as i32,
                (unique_files as f64 / total_turns as f64 * 100.0) as i32,
            );
            (
                YieldClassification::BatchingOpportunity,
                summary,
                Some(directive),
            )
        };

        YieldAdvice {
            classification,
            summary,
            resume_directive,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_batching_opportunity() {
        // 5 rounds, all different tools and files → batching opportunity.
        let turns = vec![
            (1, vec!["grep_files".to_string()], vec!["file1.rs".to_string()]),
            (
                2,
                vec!["read_file".to_string()],
                vec!["file2.rs".to_string()],
            ),
            (3, vec!["edit_file".to_string()], vec!["file3.rs".to_string()]),
            (4, vec!["run_program".to_string()], vec!["file4.rs".to_string()]),
            (5, vec!["glob_expand".to_string()], vec!["file5.rs".to_string()]),
        ];

        let advice = SerialYieldAdvisor::evaluate(&turns);
        assert_eq!(advice.classification, YieldClassification::BatchingOpportunity);
        assert!(advice.resume_directive.is_some());
        assert!(advice
            .summary
            .contains("Batching opportunity"));
    }

    #[test]
    fn test_stuck_pattern() {
        // 5 rounds, all the same tool and file (>80% repetition) → stuck.
        let turns = vec![
            (1, vec!["read_file".to_string()], vec!["file.rs".to_string()]),
            (2, vec!["read_file".to_string()], vec!["file.rs".to_string()]),
            (3, vec!["read_file".to_string()], vec!["file.rs".to_string()]),
            (4, vec!["read_file".to_string()], vec!["file.rs".to_string()]),
            (5, vec!["read_file".to_string()], vec!["file.rs".to_string()]),
        ];

        let advice = SerialYieldAdvisor::evaluate(&turns);
        assert_eq!(advice.classification, YieldClassification::StuckPattern);
        assert!(advice.resume_directive.is_none());
        assert!(advice.summary.contains("Stuck pattern"));
    }

    #[test]
    fn test_empty_audit() {
        let turns = vec![];
        let advice = SerialYieldAdvisor::evaluate(&turns);
        assert_eq!(advice.classification, YieldClassification::Unknown);
        assert!(advice.resume_directive.is_some());
    }

    #[test]
    fn resume_directive_present_iff_not_stuck() {
        // Engine contract (src/engine.rs interactive serial-chain-yield path):
        // aish RESUMES the turn in place whenever the advisor produced a
        // resume_directive, and only escalates the yield banner to the operator
        // for a StuckPattern. Lock that invariant here so a future advisor change
        // can't silently turn a resumable yield into an operator-facing stall.

        // Batching opportunity → has a directive (resumes).
        let batching = SerialYieldAdvisor::evaluate(&[
            (1, vec!["grep_files".to_string()], vec!["a.rs".to_string()]),
            (2, vec!["read_file".to_string()], vec!["b.rs".to_string()]),
            (3, vec!["edit_file".to_string()], vec!["c.rs".to_string()]),
        ]);
        assert_eq!(
            batching.classification,
            YieldClassification::BatchingOpportunity
        );
        assert!(batching.resume_directive.is_some());

        // Unknown (empty audit) → has a directive (resumes, per the fix).
        let unknown = SerialYieldAdvisor::evaluate(&[]);
        assert_eq!(unknown.classification, YieldClassification::Unknown);
        assert!(unknown.resume_directive.is_some());

        // Stuck → NO directive (escalates the banner to the operator).
        let stuck = SerialYieldAdvisor::evaluate(&[
            (1, vec!["read_file".to_string()], vec!["x.rs".to_string()]),
            (2, vec!["read_file".to_string()], vec!["x.rs".to_string()]),
            (3, vec!["read_file".to_string()], vec!["x.rs".to_string()]),
        ]);
        assert_eq!(stuck.classification, YieldClassification::StuckPattern);
        assert!(stuck.resume_directive.is_none());
    }

    #[test]
    fn test_mixed_pattern() {
        // 6 rounds: 4 repeated (grep) + 2 unique → still batching opportunity (67% not > 80%).
        let turns = vec![
            (1, vec!["grep_files".to_string()], vec!["file1.rs".to_string()]),
            (2, vec!["grep_files".to_string()], vec!["file1.rs".to_string()]),
            (3, vec!["grep_files".to_string()], vec!["file1.rs".to_string()]),
            (4, vec!["grep_files".to_string()], vec!["file1.rs".to_string()]),
            (
                5,
                vec!["read_file".to_string()],
                vec!["file2.rs".to_string()],
            ),
            (6, vec!["edit_file".to_string()], vec!["file3.rs".to_string()]),
        ];

        let advice = SerialYieldAdvisor::evaluate(&turns);
        assert_eq!(advice.classification, YieldClassification::BatchingOpportunity);
    }
}
