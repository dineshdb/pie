//! CEL preconditions for schedules.
//!
//! A `cron` expression answers "is it time yet". A `when` expression answers
//! "is the world in the state that makes this worth doing" — the two are
//! independent, and a schedule may use either or both.
//!
//! CEL is used rather than a bespoke condition syntax because it is
//! side-effect-free, non-Turing-complete and always terminates, so an
//! expression read out of a schedule file cannot do anything but produce a
//! value. None of the functions exposed here mutate state either; the worst a
//! malformed condition can do is refuse to fire.

use cel::{Context, Program, Value};
use chrono::{DateTime, Duration, Utc};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// A `when` expression that could not be reduced to a yes/no answer.
///
/// Every variant means the same thing to the caller — **do not run** — but they
/// are kept distinct so the reason can be reported. A condition that cannot be
/// evaluated must never be treated as satisfied.
#[derive(Debug, thiserror::Error)]
pub enum ConditionError {
    #[error("invalid CEL syntax: {0}")]
    Parse(String),
    #[error("evaluation failed: {0}")]
    Eval(String),
    #[error("expression returned {0}, not a bool")]
    NotBool(&'static str),
}

/// Inputs a `when` expression is evaluated against.
#[derive(Debug, Clone, Copy)]
pub struct ConditionContext {
    pub now: DateTime<Utc>,
    /// When this schedule last started, if it ever has.
    pub last_run: Option<DateTime<Utc>>,
}

/// How long `since` reports when a schedule has never run.
///
/// A never-run schedule reads as enormously overdue so that the natural
/// `since >= days(7)` fires the first time without the author needing to
/// special-case it with `never_run ||`.
const NEVER: i64 = 100 * 365 * 24 * 60 * 60;

fn expand_tilde(raw: &str) -> PathBuf {
    if let Some(rest) = raw.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest);
    }
    if raw == "~"
        && let Some(home) = dirs::home_dir()
    {
        return home;
    }
    PathBuf::from(raw)
}

fn type_name(value: &Value) -> &'static str {
    match value {
        Value::List(_) => "list",
        Value::Map(_) => "map",
        Value::Function(..) => "function",
        Value::Int(_) => "int",
        Value::UInt(_) => "uint",
        Value::Float(_) => "double",
        Value::String(_) => "string",
        Value::Bytes(_) => "bytes",
        Value::Bool(_) => "bool",
        Value::Duration(_) => "duration",
        Value::Timestamp(_) => "timestamp",
        Value::Opaque(_) => "opaque",
        Value::Null => "null",
    }
}

/// Register the variables and functions a `when` expression may use.
///
/// Deliberately absent: anything that runs a command. A condition is evaluated
/// outside the sandbox that governs the schedule's prompt, so a shell-out here
/// would be an unsandboxed execution path reachable from a config file. Gate
/// work on the filesystem and the clock instead, and let the prompt — which is
/// sandboxed — do the rest.
fn build_context(ctx: &ConditionContext) -> Context<'static> {
    let mut context = Context::default();

    let since = match ctx.last_run {
        Some(last) => ctx.now - last,
        None => Duration::seconds(NEVER),
    };
    let last_run = ctx.last_run.unwrap_or(DateTime::UNIX_EPOCH);

    // `add_variable` is generic over a conversion whose error is `Infallible`
    // for the `Value`s used here, so these cannot fail.
    let mut bind = |name: &str, value: Value| {
        let _: Result<(), std::convert::Infallible> = context.add_variable(name, value);
    };
    bind("now", Value::Timestamp(ctx.now.fixed_offset()));
    bind("last_run", Value::Timestamp(last_run.fixed_offset()));
    bind("since", Value::Duration(since));
    bind("never_run", Value::Bool(ctx.last_run.is_none()));

    context.add_function("exists", |path: Arc<String>| -> bool {
        expand_tilde(path.as_str()).exists()
    });
    context.add_function("days", |n: i64| -> Duration { Duration::days(n) });
    context.add_function("hours", |n: i64| -> Duration { Duration::hours(n) });
    context.add_function("minutes", |n: i64| -> Duration { Duration::minutes(n) });
    context.add_function("env", |name: Arc<String>| -> Arc<String> {
        Arc::new(std::env::var(name.as_str()).unwrap_or_default())
    });

    let now = ctx.now;
    context.add_function("age", move |path: Arc<String>| -> Duration {
        file_age(&expand_tilde(path.as_str()), now)
    });

    context
}

/// Age of a path by mtime. A missing or unreadable path reports zero rather
/// than erroring, so `age(p) >= days(7)` is false for a file that is not there
/// — pair it with `exists(p)` when the distinction matters.
fn file_age(path: &Path, now: DateTime<Utc>) -> Duration {
    let Ok(modified) = std::fs::metadata(path).and_then(|m| m.modified()) else {
        return Duration::zero();
    };
    let modified: DateTime<Utc> = modified.into();
    (now - modified).max(Duration::zero())
}

/// Evaluate a `when` expression.
///
/// Returns `Ok(true)` only for an unambiguous boolean `true`. Everything else —
/// a syntax error, a failed evaluation, or a non-boolean result such as `1 + 1`
/// — is an error, because a condition that silently fired every day would be
/// the failure that went unnoticed longest.
pub fn evaluate(expr: &str, ctx: &ConditionContext) -> Result<bool, ConditionError> {
    let program = Program::compile(expr).map_err(|e| ConditionError::Parse(e.to_string()))?;
    let context = build_context(ctx);
    match program
        .execute(&context)
        .map_err(|e| ConditionError::Eval(e.to_string()))?
    {
        Value::Bool(b) => Ok(b),
        other => Err(ConditionError::NotBool(type_name(&other))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> ConditionContext {
        ConditionContext {
            now: DateTime::from_timestamp(1_800_000_000, 0).unwrap_or(DateTime::UNIX_EPOCH),
            last_run: None,
        }
    }

    fn ran_ago(secs: i64) -> ConditionContext {
        let base = ctx();
        ConditionContext {
            last_run: Some(base.now - Duration::seconds(secs)),
            ..base
        }
    }

    #[test]
    fn plain_booleans() {
        assert_eq!(evaluate("true", &ctx()).ok(), Some(true));
        assert_eq!(evaluate("false", &ctx()).ok(), Some(false));
    }

    #[test]
    fn never_run_reads_as_overdue() {
        // The whole point of the NEVER sentinel: a first run needs no special case.
        assert_eq!(evaluate("since >= days(7)", &ctx()).ok(), Some(true));
        assert_eq!(evaluate("never_run", &ctx()).ok(), Some(true));
    }

    #[test]
    fn since_tracks_last_run() {
        let recent = ran_ago(3600);
        assert_eq!(evaluate("since >= days(7)", &recent).ok(), Some(false));
        assert_eq!(evaluate("since >= hours(1)", &recent).ok(), Some(true));
        assert_eq!(evaluate("never_run", &recent).ok(), Some(false));
    }

    #[test]
    fn now_compares_against_timestamps() {
        // ctx().now is epoch 1_800_000_000 — 2027-01-15T08:00:00Z.
        assert_eq!(
            evaluate("now >= timestamp(\"2000-01-01T00:00:00Z\")", &ctx()).ok(),
            Some(true)
        );
        assert_eq!(
            evaluate("now >= timestamp(\"2099-01-01T00:00:00Z\")", &ctx()).ok(),
            Some(false)
        );
    }

    #[test]
    fn exists_checks_the_filesystem() {
        let dir = std::env::temp_dir();
        let expr = format!("exists(\"{}\")", dir.display());
        assert_eq!(evaluate(&expr, &ctx()).ok(), Some(true));
        assert_eq!(
            evaluate("exists(\"/definitely/not/here\")", &ctx()).ok(),
            Some(false)
        );
        assert_eq!(
            evaluate("!exists(\"/definitely/not/here\")", &ctx()).ok(),
            Some(true)
        );
    }

    #[test]
    fn conjunction_and_disjunction() {
        assert_eq!(
            evaluate("never_run && exists(\"/definitely/not/here\")", &ctx()).ok(),
            Some(false)
        );
        assert_eq!(
            evaluate("never_run || exists(\"/definitely/not/here\")", &ctx()).ok(),
            Some(true)
        );
    }

    #[test]
    fn syntax_error_is_not_due() {
        let err = evaluate("now >= timestamp(\"unterminated", &ctx());
        assert!(matches!(err, Err(ConditionError::Parse(_))));
    }

    #[test]
    fn non_boolean_is_refused_not_coerced() {
        // 1 + 1 is truthy in many config languages. Here it must be an error,
        // or a typo becomes a schedule that fires every single tick.
        assert!(matches!(
            evaluate("1 + 1", &ctx()),
            Err(ConditionError::NotBool("int"))
        ));
        assert!(matches!(
            evaluate("\"yes\"", &ctx()),
            Err(ConditionError::NotBool("string"))
        ));
    }

    #[test]
    fn unknown_identifier_is_an_error() {
        assert!(evaluate("no_such_variable", &ctx()).is_err());
    }
}
