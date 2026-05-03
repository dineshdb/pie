mod types;

pub use types::*;

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::expect_used
)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn test_matches_hook_tools() {
        let hook = Hook::from(HookDef {
            name: "test".into(),
            event: HookEvent::PreToolUse,
            kind: HookType::Cmd,
            handler: "true".into(),
            matcher: Some(HookMatcher {
                tools: Some(vec!["shell".into()]),
                file_pattern: None,
            }),
            on_failure: OnFailure::Warn,
            timeout_ms: None,
            scope: HookScope::Validation,
            plugin_dir: None,
        });

        let ctx = HookContext::new(
            HookEvent::PreToolUse,
            "/".into(),
            "123".into(),
            HookContextData::Tool {
                tool: "shell".into(),
                input: json!({}),
                output: None,
            },
        );

        assert!(hook.matches(&ctx));

        let ctx_wrong = HookContext::new(
            HookEvent::PreToolUse,
            "/".into(),
            "123".into(),
            HookContextData::Tool {
                tool: "write_file".into(),
                input: json!({}),
                output: None,
            },
        );
        assert!(!hook.matches(&ctx_wrong));
    }

    #[tokio::test]
    async fn test_run_hooks_parallel_validation() {
        let hook1 = Hook::from(HookDef {
            name: "hook1".into(),
            event: HookEvent::PreToolUse,
            kind: HookType::Cmd,
            handler: "sleep 0.1 && echo hook1".into(),
            matcher: None,
            on_failure: OnFailure::Abort,
            timeout_ms: None,
            scope: HookScope::Validation,
            plugin_dir: None,
        });
        let hook2 = Hook::from(HookDef {
            name: "hook2".into(),
            event: HookEvent::PreToolUse,
            kind: HookType::Cmd,
            handler: "sleep 0.1 && echo hook2".into(),
            matcher: None,
            on_failure: OnFailure::Abort,
            timeout_ms: None,
            scope: HookScope::Validation,
            plugin_dir: None,
        });

        let ctx = HookContext::new(
            HookEvent::PreToolUse,
            "/".into(),
            "123".into(),
            HookContextData::Tool {
                tool: "shell".into(),
                input: json!({}),
                output: None,
            },
        );

        let manager = HooksManager::new(vec![hook1, hook2], None);
        let (outcomes, _) = manager.run(HookEvent::PreToolUse, &ctx).await.unwrap();

        assert_eq!(outcomes.len(), 2);
        assert!(matches!(outcomes[0], HookOutcome::Success));
        assert!(matches!(outcomes[1], HookOutcome::Success));
    }

    #[tokio::test]
    async fn test_run_hooks_sequential_transform() {
        // First transform adds a field, second transform changes it
        // We use extremely simple JSON to ensure cross-shell compatibility
        let hook1 = Hook::from(HookDef {
            name: "t1".into(),
            event: HookEvent::PreToolUse,
            kind: HookType::Cmd,
            handler: r#"printf '{"tool": "shell", "input": {"a": 1}}'"#.into(),
            matcher: None,
            on_failure: OnFailure::Abort,
            timeout_ms: None,
            scope: HookScope::Transform,
            plugin_dir: None,
        });
        let hook2 = Hook::from(HookDef {
            name: "t2".into(),
            event: HookEvent::PreToolUse,
            kind: HookType::Cmd,
            handler: r#"printf '{"tool": "shell", "input": {"a": 2}}'"#.into(),
            matcher: None,
            on_failure: OnFailure::Abort,
            timeout_ms: None,
            scope: HookScope::Transform,
            plugin_dir: None,
        });

        let ctx = HookContext::new(
            HookEvent::PreToolUse,
            "/".into(),
            "123".into(),
            HookContextData::Tool {
                tool: "shell".into(),
                input: json!({}),
                output: None,
            },
        );

        let manager = HooksManager::new(vec![hook1, hook2], None);
        let result = manager.run(HookEvent::PreToolUse, &ctx).await;

        match result {
            Ok((outcomes, data)) => {
                if outcomes.len() != 2 {
                    eprintln!("TEST FAILURE DIAGNOSTICS:");
                    eprintln!("Expected 2 outcomes, got {}", outcomes.len());
                    for (i, outcome) in outcomes.iter().enumerate() {
                        eprintln!("Outcome {i}: {outcome:?}");
                        if let HookOutcome::Error { message, .. } = outcome {
                            eprintln!("Error Message: {message}");
                        }
                    }
                }
                assert_eq!(
                    outcomes.len(),
                    2,
                    "Sequential transform should produce 2 outcomes"
                );

                let second_outcome = outcomes.get(1).expect("Should have a second outcome");
                assert!(
                    matches!(second_outcome, HookOutcome::Transformed { .. }),
                    "Second outcome should be Transformed, got: {second_outcome:?}",
                );

                assert_eq!(
                    data.get("input").and_then(|i| i.get("a")),
                    Some(&json!(2)),
                    "Final data should reflect the second transformation. Full data: {data:?}",
                );
            }
            Err(e) => panic!("Hook execution infrastructure failed: {e}"),
        }
    }
}
