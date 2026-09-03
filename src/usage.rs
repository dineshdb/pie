//! LLM usage accounting for a run: token counts, cache rate and cost.
//!
//! [`RunUsage`] is what the provider reports per interaction; [`UsageReport`]
//! is the enriched view (cache rate + cost) used for display and the JSON
//! envelope. Per-run rows land in the `llm_usage` table via
//! [`crate::session::Session::record_usage`] for bookkeeping.

use crate::config::ModelPricing;
use serde::{Deserialize, Serialize};

/// Token usage accumulated over one agent run (one interaction).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunUsage {
    /// LLM requests that reported usage.
    pub requests: u32,
    /// Prompt (input) tokens, including cached ones.
    pub prompt_tokens: i64,
    /// Completion (output) tokens.
    pub completion_tokens: i64,
    /// Total tokens as reported by the provider.
    pub total_tokens: i64,
    /// Prompt tokens served from the provider's prompt cache.
    pub cached_tokens: i64,
    /// Completion tokens spent on reasoning / chain of thought.
    pub reasoning_tokens: i64,
}

/// Token counts are bounded by context sizes; the precision loss of
/// `i64 -> f64` is far below a cent of cost.
#[allow(clippy::cast_precision_loss)]
fn to_f64(n: i64) -> f64 {
    n as f64
}

impl RunUsage {
    /// Fraction of prompt tokens served from cache (0.0–1.0). `None` when
    /// the provider reported no prompt tokens (usage unavailable).
    pub fn cache_rate(&self) -> Option<f64> {
        (self.prompt_tokens > 0).then(|| to_f64(self.cached_tokens) / to_f64(self.prompt_tokens))
    }

    /// USD cost of the run at the model's per-million-token rates.
    ///
    /// Cached prompt tokens are billed at the (cheaper) `cached_input`
    /// rate; reasoning tokens are billed at the output rate.
    pub fn cost_usd(&self, pricing: &ModelPricing) -> f64 {
        let uncached = self.prompt_tokens.saturating_sub(self.cached_tokens);
        let mill = 1_000_000.0;
        (to_f64(uncached) * pricing.input
            + to_f64(self.cached_tokens) * pricing.cached_input()
            + to_f64(self.completion_tokens) * pricing.output)
            / mill
    }

    /// One-line summary: `12.3k tokens (87% cached) · 5 requests · $0.0123`.
    /// The cache and cost parts are omitted when unavailable.
    pub fn summary(&self, cost_usd: Option<f64>) -> String {
        let mut parts = vec![format!("{} tokens", format_tokens(self.total_tokens))];
        if let Some(rate) = self.cache_rate()
            && rate > 0.0
        {
            parts.push(format!("{}% cached", (rate * 100.0).round()));
        }
        parts.push(match self.requests {
            1 => "1 request".to_string(),
            n => format!("{n} requests"),
        });
        if let Some(cost) = cost_usd {
            parts.push(format!("${cost:.4}"));
        }
        parts.join(" · ")
    }
}

impl From<agentsdk::Usage> for RunUsage {
    fn from(u: agentsdk::Usage) -> Self {
        Self {
            requests: u.requests,
            prompt_tokens: u.prompt_tokens,
            completion_tokens: u.completion_tokens,
            total_tokens: u.total_tokens,
            cached_tokens: u.cached_tokens,
            reasoning_tokens: u.reasoning_tokens,
        }
    }
}

/// Format a token count compactly: `980`, `12.3k`, `1.25M`.
fn format_tokens(n: i64) -> String {
    match n {
        0..=999 => n.to_string(),
        1_000..=999_999 => format!("{:.1}k", to_f64(n) / 1_000.0),
        _ => format!("{:.2}M", to_f64(n) / 1_000_000.0),
    }
}

/// [`RunUsage`] enriched for display and the JSON envelope.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct UsageReport {
    pub requests: u32,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub total_tokens: i64,
    pub cached_tokens: i64,
    pub reasoning_tokens: i64,
    pub cache_rate: Option<f64>,
    pub cost_usd: Option<f64>,
}

impl RunUsage {
    pub fn report(&self, cost_usd: Option<f64>) -> UsageReport {
        UsageReport {
            requests: self.requests,
            prompt_tokens: self.prompt_tokens,
            completion_tokens: self.completion_tokens,
            total_tokens: self.total_tokens,
            cached_tokens: self.cached_tokens,
            reasoning_tokens: self.reasoning_tokens,
            cache_rate: self.cache_rate(),
            cost_usd,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(requests: u32, prompt: i64, cached: i64, completion: i64) -> RunUsage {
        RunUsage {
            requests,
            prompt_tokens: prompt,
            cached_tokens: cached,
            completion_tokens: completion,
            total_tokens: prompt + completion,
            reasoning_tokens: 0,
        }
    }

    #[test]
    fn cache_rate_is_cached_over_prompt() {
        assert_eq!(usage(1, 100, 80, 50).cache_rate(), Some(0.8));
        assert_eq!(usage(1, 100, 0, 50).cache_rate(), Some(0.0));
        // Provider sent no usage at all — rate must be unknown, not zero.
        assert_eq!(RunUsage::default().cache_rate(), None);
    }

    #[test]
    fn cost_bills_cached_and_uncached_at_their_rates() {
        let pricing = ModelPricing {
            input: 1.0,
            cached_input: Some(0.1),
            output: 3.0,
        };
        // 100 uncached input + 80 cached input + 50 output
        let cost = usage(1, 180, 80, 50).cost_usd(&pricing);
        let expected = (100.0 * 1.0 + 80.0 * 0.1 + 50.0 * 3.0) / 1_000_000.0;
        assert!((cost - expected).abs() < 1e-12, "{cost} != {expected}");
    }

    #[test]
    fn cost_without_cache_discount_defaults_to_input_rate() {
        let pricing = ModelPricing {
            input: 2.0,
            cached_input: None,
            output: 0.0,
        };
        let cost = usage(1, 100, 100, 0).cost_usd(&pricing);
        assert!((cost - 100.0 * 2.0 / 1_000_000.0).abs() < 1e-12);
    }

    #[test]
    fn summary_omits_zero_cache_and_missing_cost() {
        assert_eq!(
            usage(3, 1000, 0, 200).summary(None),
            "1.2k tokens · 3 requests"
        );
        assert_eq!(
            usage(1, 1000, 0, 200).summary(None),
            "1.2k tokens · 1 request"
        );
        assert_eq!(
            usage(3, 1000, 900, 200).summary(Some(0.012_345)),
            "1.2k tokens · 90% cached · 3 requests · $0.0123"
        );
        assert_eq!(
            RunUsage::default().summary(None),
            "0 tokens · 0 requests",
            "usage unavailable must not claim 100% cached"
        );
    }

    #[test]
    fn report_carries_cache_rate_and_cost() {
        let report = usage(2, 100, 50, 10).report(Some(0.5));
        assert_eq!(report.cache_rate, Some(0.5));
        assert_eq!(report.cost_usd, Some(0.5));
        assert_eq!(report.total_tokens, 110);
    }
}
