//! TUI token usage models and display formatting.

use std::fmt;

use codex_protocol::num_format::format_with_separators;
use serde::Deserialize;
use serde::Serialize;

const BASELINE_TOKENS: i64 = 12000;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input_tokens: i64,
    pub cached_input_tokens: i64,
    pub output_tokens: i64,
    pub reasoning_output_tokens: i64,
    pub total_tokens: i64,
}

impl TokenUsage {
    pub fn is_zero(&self) -> bool {
        self.total_tokens == 0
    }

    pub(crate) fn cached_input(&self) -> i64 {
        self.cached_input_tokens.max(0)
    }

    pub(crate) fn non_cached_input(&self) -> i64 {
        (self.input_tokens - self.cached_input()).max(0)
    }

    pub(crate) fn blended_total(&self) -> i64 {
        (self.non_cached_input() + self.output_tokens.max(0)).max(0)
    }

    /// Returns the raw `total_tokens` value. For `last_token_usage`, this is the latest active
    /// context size; for `total_token_usage`, this is the accumulated session total.
    pub(crate) fn tokens_in_context_window(&self) -> i64 {
        self.total_tokens
    }

    pub(crate) fn percent_of_context_window_remaining(&self, context_window: i64) -> i64 {
        if context_window <= BASELINE_TOKENS {
            return 0;
        }
        let effective_window = context_window - BASELINE_TOKENS;
        let used = (self.tokens_in_context_window() - BASELINE_TOKENS).max(0);
        let remaining = (effective_window - used).max(0);
        ((remaining as f64 / effective_window as f64) * 100.0)
            .clamp(0.0, 100.0)
            .round() as i64
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct TokenUsageInfo {
    pub(crate) total_token_usage: TokenUsage,
    pub(crate) last_token_usage: TokenUsage,
    pub(crate) model_context_window: Option<i64>,
}

impl fmt::Display for TokenUsage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Token usage: total={} input={}{} output={}{}",
            format_with_separators(self.blended_total()),
            format_with_separators(self.non_cached_input()),
            if self.cached_input() > 0 {
                format!(
                    " (+ {} cached)",
                    format_with_separators(self.cached_input())
                )
            } else {
                String::new()
            },
            format_with_separators(self.output_tokens),
            if self.reasoning_output_tokens > 0 {
                format!(
                    " (reasoning {})",
                    format_with_separators(self.reasoning_output_tokens)
                )
            } else {
                String::new()
            }
        )
    }
}

/// Per-turn differences of reported cumulative thread totals. Output already includes reasoning.
/// A resumed turn without a pre-turn baseline deliberately has no displayable count.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct TurnTokenUsage {
    turn_id: Option<String>,
    latest: Option<(i64, i64)>,
    baseline: Option<(i64, i64)>,
    counts: Option<(i64, i64)>,
    completed: std::collections::HashMap<String, Option<(i64, i64)>>,
    latest_context: Option<i64>,
    context_baseline: Option<i64>,
    completed_context_growth: std::collections::HashMap<String, Option<i64>>,
}

impl TurnTokenUsage {
    pub(crate) fn seed_empty_thread(&mut self) {
        if self.turn_id.is_none() && self.latest.is_none() {
            self.latest = Some((0, 0));
        }
    }

    pub(crate) fn start(&mut self, id: &str) {
        if self.turn_id.as_deref() != Some(id) {
            self.turn_id = Some(id.to_string());
            self.baseline = self.latest;
            self.context_baseline = self.latest_context.take();
            self.counts = None;
        }
    }

    pub(crate) fn resume(&mut self, id: &str) {
        if self.turn_id.as_deref() != Some(id) {
            self.turn_id = Some(id.to_string());
            self.baseline = None;
            self.counts = None;
            self.context_baseline = None;
            self.latest_context = None;
        }
    }

    pub(crate) fn observe(&mut self, id: &str, input: i64, output: i64) -> bool {
        if self.turn_id.as_deref().is_some_and(|current| current != id) {
            return false; // A delayed report from another turn must not replace the active counters.
        }
        if self.turn_id.is_none() {
            // Missing start (resume/reconnect): never infer turn usage from session totals.
            self.turn_id = Some(id.to_string());
            self.baseline = None;
            self.counts = None;
            self.context_baseline = None;
            self.latest_context = None;
        }
        let total = (input.max(0), output.max(0));
        if self
            .latest
            .is_some_and(|last| total.0 < last.0 || total.1 < last.1)
        {
            return false; // Stale/repeated/out-of-order cumulative notifications cannot subtract usage.
        }
        self.latest = Some(total);
        if let Some(base) = self.baseline {
            self.counts = Some((
                total.0.saturating_sub(base.0),
                total.1.saturating_sub(base.1),
            ));
        }
        true
    }

    /// The latest request's total is the reported active context, not cumulative input usage.
    /// Context can shrink after compaction even while cumulative usage continues to increase.
    pub(crate) fn observe_with_context(&mut self, id: &str, input: i64, output: i64, context: i64) {
        if self.observe(id, input, output) {
            self.latest_context = (context >= 0).then_some(context);
        }
    }

    pub(crate) fn snapshot_context_growth(&mut self, id: &str) -> Option<i64> {
        let growth = (self.turn_id.as_deref() == Some(id))
            .then(|| {
                self.latest_context
                    .zip(self.context_baseline)
                    .map(|(latest, baseline)| latest.saturating_sub(baseline))
            })
            .flatten();
        *self
            .completed_context_growth
            .entry(id.to_owned())
            .or_insert(growth)
    }

    pub(crate) fn snapshot_counts(&mut self, id: &str) -> Option<(i64, i64)> {
        let counts = (self.turn_id.as_deref() == Some(id))
            .then_some(self.counts)
            .flatten();
        *self.completed.entry(id.to_owned()).or_insert(counts)
    }

    pub(crate) fn counts(&self) -> Option<(i64, i64)> {
        self.counts
    }
}

pub(crate) fn compact_turn_tokens(count: i64) -> String {
    if count >= 1_000_000 {
        format!("{:.1}M", count as f64 / 1_000_000.0)
    } else if count >= 1_000 {
        format!("{:.1}k", count as f64 / 1_000.0)
    } else {
        count.to_string()
    }
}

#[cfg(test)]
mod turn_token_tests {
    use super::*;

    #[test]
    fn cumulative_requests_repeats_reasoning_and_new_turn() {
        let mut usage = TurnTokenUsage::default();
        usage.seed_empty_thread();
        usage.start("first");
        assert_eq!(usage.counts(), None); // No made-up realtime counts.
        usage.observe("first", 500, 128); // Output includes the provider's reasoning tokens.
        usage.observe("first", 500, 128);
        assert_eq!(usage.counts(), Some((500, 128)));
        usage.observe("first", 1_100, 300);
        usage.start("first"); // Repeated start/reconnect preserves the baseline.
        usage.observe("first", 500, 128); // stale notification
        usage.observe("unrelated-turn", 10_000, 9_000);
        assert_eq!(usage.counts(), Some((1_100, 300)));
        usage.start("second");
        assert_eq!(usage.counts(), None);
        usage.observe("second", 1_600, 428);
        assert_eq!(usage.counts(), Some((500, 128)));
    }

    #[test]
    fn resume_without_baseline_and_missing_usage_are_unknown() {
        let mut usage = TurnTokenUsage::default();
        usage.observe("resumed", 42_300, 900);
        usage.start("resumed");
        assert_eq!(usage.counts(), None);
        usage.start("next");
        usage.observe("next", 42_800, 1_028);
        assert_eq!(usage.counts(), Some((500, 128)));
        usage.resume("missed-start");
        usage.observe("missed-start", 43_000, 1_100);
        assert_eq!(usage.counts(), None);
        assert_eq!(compact_turn_tokens(42_300), "42.3k");
        assert_eq!(compact_turn_tokens(2_500_000), "2.5M");
    }
}

#[cfg(test)]
mod shell_grouping_token_tests {
    use super::*;
    #[test]
    fn shell_grouping_usage_snapshot_requests_repeats_resume_and_thread_clone() {
        let mut usage = TurnTokenUsage::default();
        usage.seed_empty_thread();
        usage.start("first");
        usage.observe("first", 500, 128);
        usage.observe("first", 500, 128);
        usage.observe("first", 12_400, 2_100);
        assert_eq!(usage.snapshot_counts("first"), Some((12_400, 2_100)));
        usage.start("second");
        usage.observe("second", 12_900, 2_228);
        let mut switched = usage.clone();
        assert_eq!(switched.snapshot_counts("first"), Some((12_400, 2_100)));
        assert_eq!(switched.snapshot_counts("second"), Some((500, 128)));
        let mut resumed = TurnTokenUsage::default();
        resumed.resume("old");
        resumed.observe("old", 42_000, 3_000);
        assert_eq!(resumed.snapshot_counts("old"), None);
    }
}

#[cfg(test)]
mod context_growth_tests {
    use super::*;

    #[test]
    fn context_growth_uses_last_request_and_preserves_completed_snapshots() {
        let mut usage = TurnTokenUsage::default();
        usage.seed_empty_thread();
        usage.start("first");
        usage.observe_with_context("first", 40_000, 1_000, 21_000);
        assert_eq!(usage.snapshot_context_growth("first"), None);
        usage.start("second");
        usage.observe_with_context("second", 80_000, 2_000, 24_200);
        usage.start("second"); // Reconnect must not reset the baseline.
        usage.observe_with_context("second", 120_000, 3_000, 25_300);
        usage.observe_with_context("second", 120_000, 3_000, 25_300); // Repeat.
        usage.observe_with_context("second", 80_000, 2_000, 90_000); // Stale total.
        usage.observe_with_context("first", 200_000, 4_000, 90_000); // Wrong turn.
        assert_eq!(usage.snapshot_counts("second"), Some((80_000, 2_000)));
        assert_eq!(usage.snapshot_context_growth("second"), Some(4_300));
        usage.observe_with_context("second", 140_000, 3_100, 26_000);
        assert_eq!(usage.snapshot_context_growth("second"), Some(4_300));
        usage.start("third");
        usage.observe_with_context("third", 160_000, 3_200, 26_000);
        let mut switched = usage.clone();
        assert_eq!(switched.snapshot_context_growth("second"), Some(4_300));
        assert_eq!(switched.snapshot_context_growth("third"), Some(0));
    }

    #[test]
    fn context_growth_allows_compaction_with_unchanged_cumulative_usage() {
        let mut usage = TurnTokenUsage::default();
        usage.observe_with_context("previous", 40_000, 1_000, 30_000);
        usage.start("compacted");
        usage.observe_with_context("compacted", 50_000, 1_100, 31_000);
        usage.observe_with_context("compacted", 50_000, 1_100, 10_000);
        assert_eq!(usage.snapshot_context_growth("compacted"), Some(-20_000));
    }

    #[test]
    fn context_growth_missing_reports_and_resumed_baselines_stay_unknown() {
        let mut usage = TurnTokenUsage::default();
        usage.observe_with_context("old", 40_000, 1_000, 30_000);
        usage.resume("resumed");
        usage.observe_with_context("resumed", 50_000, 1_100, 31_000);
        assert_eq!(usage.snapshot_context_growth("resumed"), None);
        usage.start("missing-report");
        assert_eq!(usage.snapshot_context_growth("missing-report"), None);
        usage.start("after-missing");
        usage.observe_with_context("after-missing", 60_000, 1_200, 32_000);
        assert_eq!(usage.snapshot_context_growth("after-missing"), None);
        usage.start("invalid-report");
        usage.observe_with_context("invalid-report", 70_000, 1_300, -1);
        assert_eq!(usage.snapshot_context_growth("invalid-report"), None);
    }
}
