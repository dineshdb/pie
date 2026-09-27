use agentsdk::core::agent::{PostToolAction, PreToolAction};
use agentsdk::{AgentPlugin, PluginContext};
use async_trait::async_trait;
use serde_json::Value;
use std::time::Instant;

const MAX_AGE_RECENT: std::time::Duration = std::time::Duration::from_secs(5);
const MAX_AGE_WINDOW: std::time::Duration = std::time::Duration::from_secs(20);
const MAX_REPEATS: usize = 2;

/// Tools that change something. Any other tool counts toward the
/// read-only streak — a run that only ever reads is researching, not
/// working.
const MUTATING_TOOLS: [&str; 2] = ["Edit", "Write"];

/// Read-only calls allowed before the first convergence nudge, and the
/// spacing of every nudge after. 12 ≈ a dozen one-question greps — the
/// open-ended-task failure mode is 50+ of these in a row, each costing
/// a full model turn.
const NUDGE_FIRST_AT: usize = 12;
const NUDGE_EVERY: usize = 8;

/// The nudge preempts the stream plugin's clamp/redact for the one
/// result it extends, so it clamps the base itself.
const NUDGE_BASE_MAX_CHARS: usize = 2_000;

struct CallEntry {
    name: String,
    args: Value,
    at: Instant,
}

pub struct DoomLoopPlugin {
    calls: Vec<CallEntry>,
    /// Completed read-only tool calls since the last mutation.
    reads_since_mutation: usize,
}

impl DoomLoopPlugin {
    pub fn new() -> Self {
        Self {
            calls: Vec::new(),
            reads_since_mutation: 0,
        }
    }

    /// The nudge due at this streak length, if any: first at
    /// [`NUDGE_FIRST_AT`], then every [`NUDGE_EVERY`].
    fn nudge_due(reads: usize) -> bool {
        reads == NUDGE_FIRST_AT
            || (reads > NUDGE_FIRST_AT && (reads - NUDGE_FIRST_AT).is_multiple_of(NUDGE_EVERY))
    }
}

#[async_trait]
impl AgentPlugin for DoomLoopPlugin {
    fn name(&self) -> &'static str {
        "doom_loop"
    }

    async fn on_tool_pre_execute(
        &mut self,
        _ctx: &mut PluginContext,
        _id: &str,
        name: &str,
        arguments: &Value,
    ) -> PreToolAction {
        let now = Instant::now();
        let same: Vec<_> = self
            .calls
            .iter()
            .filter(|c| c.name == name && c.args == *arguments)
            .collect();

        if let Some(last) = same.last() {
            let recency = now.duration_since(last.at) < MAX_AGE_RECENT;
            let frequency = same.len() > MAX_REPEATS
                && same
                    .iter()
                    .filter(|c| now.duration_since(c.at) < MAX_AGE_WINDOW)
                    .count()
                    > MAX_REPEATS;

            if recency && frequency {
                tracing::debug!(tool = name, "aborting duplicate tool call");
                return PreToolAction::Abort("You already made that tool call.".to_string());
            }
        }

        self.calls.push(CallEntry {
            name: name.to_string(),
            args: arguments.clone(),
            at: now,
        });
        PreToolAction::Proceed(None)
    }

    async fn on_tool_post_execute(
        &mut self,
        _ctx: &mut PluginContext,
        _id: &str,
        name: &str,
        result: &Result<Value, String>,
    ) -> PostToolAction {
        if MUTATING_TOOLS.contains(&name) {
            self.reads_since_mutation = 0;
            return PostToolAction::Proceed(None);
        }
        self.reads_since_mutation += 1;

        // Nagging rides the tool result: the model reads it as part of
        // the output it asked for. Failures carry no payload worth
        // extending and a nudge must not bury an error.
        let Ok(value) = result else {
            return PostToolAction::Proceed(None);
        };
        if !Self::nudge_due(self.reads_since_mutation) {
            return PostToolAction::Proceed(None);
        }
        tracing::info!(
            reads = self.reads_since_mutation,
            tool = name,
            "nudging: read-only streak, no change made yet"
        );
        let nudge = format!(
            "\n\n[convergence] {} read-only tool calls since your last change. Converge: make the \
             smallest edit that completes the task, or — if the research says nothing should \
             change — report your findings and finish.",
            self.reads_since_mutation
        );
        let mut base = match value {
            Value::String(s) => jewels::redact(s).into_owned(),
            other => jewels::redact(&other.to_string()).into_owned(),
        };
        if let Some((cut, _)) = base.char_indices().nth(NUDGE_BASE_MAX_CHARS) {
            base.truncate(cut);
            base.push('…');
        }
        base.push_str(&nudge);
        PostToolAction::Proceed(Some(Value::String(base)))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

    use super::*;

    fn plugin() -> DoomLoopPlugin {
        DoomLoopPlugin::new()
    }

    fn ctx() -> PluginContext {
        let mut world = agentsdk::hecs::World::new();
        let entity = world.spawn(());
        PluginContext::new(world, entity)
    }

    async fn run(plugin: &mut DoomLoopPlugin, tool: &str) -> PostToolAction {
        plugin
            .on_tool_post_execute(&mut ctx(), "1", tool, &Ok(Value::String("out".into())))
            .await
    }

    #[tokio::test]
    async fn no_nudge_below_the_threshold() {
        let mut p = plugin();
        for _ in 0..NUDGE_FIRST_AT - 1 {
            let action = run(&mut p, "Read").await;
            assert!(matches!(action, PostToolAction::Proceed(None)));
        }
    }

    #[tokio::test]
    async fn nudge_fires_at_the_threshold_with_the_streak_count() {
        let mut p = plugin();
        for _ in 0..NUDGE_FIRST_AT - 1 {
            let _ = run(&mut p, "Read").await;
        }
        match run(&mut p, "Bash").await {
            PostToolAction::Proceed(Some(Value::String(text))) => {
                assert!(text.starts_with("out"), "original result is preserved");
                assert!(text.contains("12 read-only tool calls"));
            }
            other => panic!("expected an extended result, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_mutation_resets_the_streak() {
        let mut p = plugin();
        for _ in 0..NUDGE_FIRST_AT - 1 {
            let _ = run(&mut p, "Read").await;
        }
        let _ = run(&mut p, "Edit").await;
        let action = run(&mut p, "Read").await;
        assert!(matches!(action, PostToolAction::Proceed(None)));
    }

    #[tokio::test]
    async fn repeat_nudges_arrive_every_nudge_every_calls() {
        let mut p = plugin();
        for _ in 0..NUDGE_FIRST_AT + NUDGE_EVERY - 1 {
            let _ = run(&mut p, "Read").await;
        }
        match run(&mut p, "Glob").await {
            PostToolAction::Proceed(Some(Value::String(text))) => {
                assert!(text.contains("20 read-only tool calls"));
            }
            other => panic!("expected a repeat nudge, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn failures_are_never_extended() {
        let mut p = plugin();
        for _ in 0..NUDGE_FIRST_AT {
            let _ = p
                .on_tool_post_execute(&mut ctx(), "1", "Read", &Ok(Value::String("out".into())))
                .await;
        }
        let action = p
            .on_tool_post_execute(&mut ctx(), "1", "Read", &Err("boom".into()))
            .await;
        assert!(matches!(action, PostToolAction::Proceed(None)));
    }
}
