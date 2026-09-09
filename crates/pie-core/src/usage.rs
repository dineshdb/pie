//! LLM usage accounting for a run: token counts, cache rate and cost.
//!
//! [`RunUsage`] is what the provider reports per interaction; [`UsageReport`]
//! is the enriched view (cache rate + cost) used for display and the JSON
//! envelope. Per-run rows land in the `llm_usage` table via
//! [`crate::session::Session::record_usage`] for bookkeeping, and
//! [`by_model`] aggregates that table for the `pie usage` report.

use crate::config::ModelPricing;
use crate::db::DbPool;
use crate::error::Result;
use serde::{Deserialize, Serialize};
use sqlx::Row as _;

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

/// One aggregated `pie usage` report row: per-model usage over a window.
#[derive(Debug, Clone, Serialize)]
pub struct ModelUsage {
    pub model: String,
    #[serde(flatten)]
    pub usage: UsageReport,
}

/// Aggregate `llm_usage` rows per model within a window: `since_ms = 0`
/// means all time. Models without configured pricing report a `null` cost.
/// Rows are ordered by cost (unpriced last), then by token volume.
pub async fn by_model(pool: &DbPool, since_ms: i64) -> Result<Vec<ModelUsage>> {
    let rows = sqlx::query(
        "SELECT model, COUNT(*) AS requests, SUM(prompt_tokens) AS prompt, \
         SUM(completion_tokens) AS completion, SUM(cached_tokens) AS cached, \
         SUM(reasoning_tokens) AS reasoning, SUM(total_tokens) AS total, \
         SUM(cost_usd) AS cost \
         FROM llm_usage WHERE (? = 0 OR ts >= ?) \
         GROUP BY model \
         ORDER BY cost IS NULL, cost DESC, total DESC",
    )
    .bind(since_ms)
    .bind(since_ms)
    .fetch_all(pool)
    .await?;

    rows.into_iter()
        .map(|row| {
            let prompt_tokens: i64 = row.try_get("prompt")?;
            let cached_tokens: i64 = row.try_get("cached")?;
            Ok(ModelUsage {
                model: row.try_get("model")?,
                usage: UsageReport {
                    requests: row.try_get::<i64, _>("requests")?.try_into().map_err(
                        |e: std::num::TryFromIntError| {
                            crate::error::AppError::Db(sqlx::Error::Decode(Box::new(e)))
                        },
                    )?,
                    prompt_tokens,
                    completion_tokens: row.try_get("completion")?,
                    total_tokens: row.try_get("total")?,
                    cached_tokens,
                    reasoning_tokens: row.try_get("reasoning")?,
                    cache_rate: (prompt_tokens > 0)
                        .then(|| to_f64(cached_tokens) / to_f64(prompt_tokens)),
                    cost_usd: row.try_get("cost")?,
                },
            })
        })
        .collect()
}

/// Grand total across report rows. The cost sums the models that have
/// pricing; it is `None` when no row was priced. Zero-usage sessions of
/// unpriced models don't hide priced ones.
#[must_use]
pub fn totals(rows: &[ModelUsage]) -> UsageReport {
    let mut t = UsageReport {
        requests: 0,
        prompt_tokens: 0,
        completion_tokens: 0,
        total_tokens: 0,
        cached_tokens: 0,
        reasoning_tokens: 0,
        cache_rate: None,
        cost_usd: None,
    };
    for row in rows {
        let u = row.usage;
        t.requests += u.requests;
        t.prompt_tokens += u.prompt_tokens;
        t.completion_tokens += u.completion_tokens;
        t.total_tokens += u.total_tokens;
        t.cached_tokens += u.cached_tokens;
        t.reasoning_tokens += u.reasoning_tokens;
        t.cost_usd = match (t.cost_usd, u.cost_usd) {
            (acc, None) => acc,
            (None, Some(c)) => Some(c),
            (Some(acc), Some(c)) => Some(acc + c),
        };
    }
    t.cache_rate = (t.prompt_tokens > 0).then(|| to_f64(t.cached_tokens) / to_f64(t.prompt_tokens));
    t
}

/// Render the report as an aligned table. `days = 0` reads as all time.
#[must_use]
pub fn render_report(rows: &[ModelUsage], days: u32) -> String {
    let window = if days == 0 {
        "all time".to_string()
    } else {
        format!("last {days} days")
    };
    if rows.is_empty() {
        return format!("no LLM usage recorded ({window})\n");
    }

    let header = [
        "model", "req", "prompt", "compl", "total", "cached", "cache", "cost",
    ];
    let cells: Vec<[String; 8]> = rows
        .iter()
        .map(|r| {
            [
                r.model.clone(),
                r.usage.requests.to_string(),
                format_tokens(r.usage.prompt_tokens),
                format_tokens(r.usage.completion_tokens),
                format_tokens(r.usage.total_tokens),
                format_tokens(r.usage.cached_tokens),
                pct_cell(r.usage.cache_rate),
                cost_cell(r.usage.cost_usd),
            ]
        })
        .collect();
    let total = totals(rows);
    let total_row = [
        "total".to_string(),
        total.requests.to_string(),
        format_tokens(total.prompt_tokens),
        format_tokens(total.completion_tokens),
        format_tokens(total.total_tokens),
        format_tokens(total.cached_tokens),
        pct_cell(total.cache_rate),
        cost_cell(total.cost_usd),
    ];

    let widths: Vec<usize> = (0..header.len())
        .map(|i| {
            let header_len = header.get(i).map_or(0, |h| h.chars().count());
            let body_max = cells
                .iter()
                .map(|row| row.get(i).map_or(0, |c| c.chars().count()))
                .max()
                .unwrap_or(0);
            let total_len = total_row.get(i).map_or(0, |c| c.chars().count());
            header_len.max(body_max).max(total_len)
        })
        .collect();

    let mut out = format!("LLM usage, {window}\n\n");
    let fmt_cell = |i: usize, c: &str| {
        let width = widths.get(i).copied().unwrap_or(0);
        if i == 0 {
            format!("{c:<width$}")
        } else {
            format!("{c:>width$}")
        }
    };
    let row_line = |row: &[String; 8]| -> String {
        row.iter()
            .enumerate()
            .map(|(i, c)| fmt_cell(i, c))
            .collect::<Vec<_>>()
            .join("  ")
    };

    let header_line = header
        .iter()
        .enumerate()
        .map(|(i, h)| fmt_cell(i, h))
        .collect::<Vec<_>>()
        .join("  ");
    let rule = "─".repeat(header_line.chars().count());
    out.push_str(&header_line);
    out.push('\n');
    for row in &cells {
        out.push_str(&row_line(row));
        out.push('\n');
    }
    out.push_str(&rule);
    out.push('\n');
    out.push_str(&row_line(&total_row));
    out.push('\n');
    out
}

fn pct_cell(rate: Option<f64>) -> String {
    match rate {
        Some(r) => format!("{}%", (r * 100.0).round()),
        None => "—".to_string(),
    }
}

fn cost_cell(cost: Option<f64>) -> String {
    match cost {
        Some(c) => format!("${c:.4}"),
        None => "—".to_string(),
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

    async fn insert_run(
        pool: &DbPool,
        session_id: &str,
        ts: i64,
        model: &str,
        cost: Option<f64>,
    ) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO llm_usage (session_id, ts, model, agent, requests, prompt_tokens, \
             completion_tokens, cached_tokens, reasoning_tokens, total_tokens, cost_usd) \
             VALUES (?, ?, ?, NULL, 1, 100, 50, 80, 20, 150, ?)",
        )
        .bind(session_id)
        .bind(ts)
        .bind(model)
        .bind(cost)
        .execute(pool)
        .await?;
        Ok(())
    }

    #[tokio::test]
    async fn by_model_aggregates_within_window() -> anyhow::Result<()> {
        use std::sync::Arc;

        let pool = Arc::new(crate::db::create_test_pool().await?);
        let session =
            crate::session::Session::create(pool.clone(), std::path::Path::new("/test")).await?;
        let sid = session.id.to_string();
        let now = chrono::Utc::now().timestamp_millis();

        // priced model, two runs (one recent, one old)
        insert_run(&pool, &sid, now - 1_000, "m1", Some(0.01)).await?;
        insert_run(&pool, &sid, now - 40 * 86_400_000, "m1", Some(0.02)).await?;
        // unpriced model, recent — cost stays NULL
        insert_run(&pool, &sid, now - 1_000, "m2", None).await?;

        // all time: both models, m1 aggregated, ordered by cost (unpriced last)
        let rows = by_model(&pool, 0).await?;
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].model, "m1");
        assert_eq!(rows[0].usage.requests, 2);
        assert_eq!(rows[0].usage.prompt_tokens, 200);
        assert_eq!(rows[0].usage.total_tokens, 300);
        assert_eq!(rows[0].usage.cache_rate, Some(0.8));
        assert_eq!(rows[0].usage.cost_usd, Some(0.03));
        assert_eq!(rows[1].model, "m2");
        assert_eq!(rows[1].usage.cost_usd, None, "unpriced model has null cost");

        // window excludes the 40-day-old run
        let rows = by_model(&pool, now - 30 * 86_400_000).await?;
        assert_eq!(rows.len(), 2, "m2 and the recent m1 run");
        assert_eq!(rows[0].usage.requests, 1);
        Ok(())
    }

    #[test]
    fn totals_sum_known_costs_and_cache_rate() {
        let rows = vec![
            ModelUsage {
                model: "a".into(),
                usage: usage(2, 100, 80, 50).report(Some(0.02)),
            },
            ModelUsage {
                model: "b".into(),
                usage: usage(1, 100, 0, 50).report(None),
            },
        ];
        let t = totals(&rows);
        assert_eq!(t.requests, 3);
        assert_eq!(t.prompt_tokens, 200);
        assert_eq!(t.total_tokens, 300);
        assert_eq!(t.cache_rate, Some(0.4));
        assert_eq!(t.cost_usd, Some(0.02), "unpriced models don't zero the sum");
    }

    #[test]
    fn render_report_shows_cells_totals_and_unpriced_marker() {
        let rows = vec![ModelUsage {
            model: "glm-5.1".into(),
            usage: usage(1, 1000, 900, 200).report(Some(0.0123)),
        }];
        let out = render_report(&rows, 30);
        assert!(out.starts_with("LLM usage, last 30 days\n\nmodel"), "{out}");
        assert!(out.contains("glm-5.1"), "{out}");
        assert!(out.contains("$0.0123"), "{out}");
        assert!(out.contains("90%"), "{out}");
        assert!(out.contains("total"), "{out}");
        assert!(out.contains('─'), "separator rule missing: {out}");

        let unpriced = render_report(
            &[ModelUsage {
                model: "m".into(),
                usage: usage(1, 10, 0, 5).report(None),
            }],
            0,
        );
        assert!(
            unpriced.contains('—'),
            "unpriced cost must render as —: {unpriced}"
        );
        assert!(unpriced.contains("all time"), "{unpriced}");
    }

    #[test]
    fn render_report_empty_window() {
        assert_eq!(
            render_report(&[], 30),
            "no LLM usage recorded (last 30 days)\n"
        );
    }
}
