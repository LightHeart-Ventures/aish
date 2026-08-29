/// TASK-360: Turn-completion recap feature.
///
/// When a turn finishes successfully (final answer produced, no forced-summarize/loop/etc),
/// optionally append a short recap of what was completed to give the user a sense of closure
/// and completion. This is gated by a session flag and emitted to stderr (below the answer)
/// as a dim line for visibility without distraction.

use crate::context::Usage;
use std::time::Duration;

/// Configuration for turn-completion recap output.
#[derive(Clone, Debug)]
#[allow(dead_code)]
pub struct RecapConfig {
    /// Enable turn-completion recaps.
    pub enabled: bool,
    /// Show tool-call summary (count, tool names).
    pub show_tools: bool,
    /// Show token usage (in/out, cache hit rate).
    pub show_usage: bool,
    /// Show elapsed time.
    pub show_duration: bool,
}

impl Default for RecapConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            show_tools: true,
            show_usage: false, // off by default to keep it lightweight
            show_duration: true,
        }
    }
}

/// Recap data collected during a turn.
#[allow(dead_code)]
pub struct TurnRecapData {
    /// Total tool calls executed this turn.
    pub tool_count: usize,
    /// Tool calls bucketed by name (for summary).
    pub tools_by_name: std::collections::HashMap<String, usize>,
    /// Total token usage for the turn (if available from backend).
    pub usage: Option<Usage>,
    /// Elapsed time for the turn.
    pub elapsed: Duration,
}

impl TurnRecapData {
    #[allow(dead_code)]
    pub fn new() -> Self {
        Self {
            tool_count: 0,
            tools_by_name: Default::default(),
            usage: None,
            elapsed: Duration::ZERO,
        }
    }

    /// Record a tool call.
    #[allow(dead_code)]
    pub fn record_tool(&mut self, name: &str) {
        self.tool_count += 1;
        *self.tools_by_name.entry(name.to_string()).or_insert(0) += 1;
    }

    /// Build the recap text.
    #[allow(dead_code)]
    pub fn format_recap(&self, config: &RecapConfig) -> String {
        if !config.enabled || (self.tool_count == 0 && self.usage.is_none()) {
            return String::new();
        }

        let mut parts = Vec::new();

        // Tool summary
        if config.show_tools && self.tool_count > 0 {
            let tools_desc = if self.tools_by_name.len() == 1 {
                let (name, count) = self.tools_by_name.iter().next().unwrap();
                if *count == 1 {
                    format!("1 × {name}")
                } else {
                    format!("{} × {name}", count)
                }
            } else {
                let mut sorted: Vec<_> = self.tools_by_name.iter().collect();
                sorted.sort_by_key(|&(_, count)| std::cmp::Reverse(*count));
                let summary = sorted
                    .into_iter()
                    .map(|(name, count)| format!("{} {}", count, name))
                    .collect::<Vec<_>>()
                    .join(" + ");
                format!("{} tools: {}", self.tool_count, summary)
            };
            parts.push(format!("🛠️  {tools_desc}"));
        }

        // Usage summary
        if config.show_usage {
            if let Some(usage) = &self.usage {
                let cache_pct = if usage.input_tokens > 0 {
                    (usage.cache_read_tokens as f64 / usage.input_tokens as f64) * 100.0
                } else {
                    0.0
                };
                let usage_desc = if usage.cache_read_tokens > 0 {
                    format!(
                        "{} in ({}% cached) + {} out",
                        usage.input_tokens, cache_pct as u32, usage.output_tokens
                    )
                } else {
                    format!("{} in + {} out", usage.input_tokens, usage.output_tokens)
                };
                parts.push(format!("📊 {usage_desc}"));
            }
        }

        // Duration summary
        if config.show_duration {
            let secs = self.elapsed.as_secs_f64();
            let duration_desc = if secs < 1.0 {
                format!("{:.0}ms", secs * 1000.0)
            } else if secs < 60.0 {
                format!("{:.1}s", secs)
            } else {
                let mins = secs / 60.0;
                format!("{:.1}m", mins)
            };
            parts.push(format!("⏱️  {duration_desc}"));
        }

        if parts.is_empty() {
            String::new()
        } else {
            parts.join(" · ")
        }
    }
}

/// Emit a turn-completion recap if enabled.
#[allow(dead_code)]
pub fn emit_recap(recap: &TurnRecapData, config: &RecapConfig) {
    let text = recap.format_recap(config);
    if !text.is_empty() {
        eprintln!("\x1b[2m  recap: {text}\x1b[0m");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_recap_formatting() {
        let mut data = TurnRecapData::new();
        data.record_tool("read_file");
        data.record_tool("read_file");
        data.record_tool("edit_file");
        data.elapsed = Duration::from_secs(5);

        let config = RecapConfig {
            enabled: true,
            show_tools: true,
            show_usage: false,
            show_duration: true,
        };

        let recap = data.format_recap(&config);
        assert!(recap.contains("read_file"));
        assert!(recap.contains("edit_file"));
        assert!(recap.contains("5.0s"));
    }

    #[test]
    fn test_recap_disabled() {
        let mut data = TurnRecapData::new();
        data.record_tool("read_file");

        let config = RecapConfig {
            enabled: false,
            ..Default::default()
        };

        let recap = data.format_recap(&config);
        assert!(recap.is_empty());
    }
}
