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
            strategy: ExecutionStrategy::Sequential,
            plugin_dir: None,
        });

        let ctx = HookContext::new(
            HookEvent::PreToolUse,
            "/".into(),
            "123".into(),
            HookContextData::Tool(ToolData {
                tool: Some("shell".into()),
                input: Some(json!({})),
                output: None,
            }),
        );

        assert!(hook.matches(&ctx));

        let ctx_wrong = HookContext::new(
            HookEvent::PreToolUse,
            "/".into(),
            "123".into(),
            HookContextData::Tool(ToolData {
                tool: Some("write_file".into()),
                input: Some(json!({})),
                output: None,
            }),
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
            strategy: ExecutionStrategy::Sequential,
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
            strategy: ExecutionStrategy::Sequential,
            plugin_dir: None,
        });

        let ctx = HookContext::new(
            HookEvent::PreToolUse,
            "/".into(),
            "123".into(),
            HookContextData::Tool(ToolData {
                tool: Some("shell".into()),
                input: Some(json!({})),
                output: None,
            }),
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
            strategy: ExecutionStrategy::Sequential,
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
            strategy: ExecutionStrategy::Sequential,
            plugin_dir: None,
        });

        let ctx = HookContext::new(
            HookEvent::PreToolUse,
            "/".into(),
            "123".into(),
            HookContextData::Tool(ToolData {
                tool: Some("shell".into()),
                input: Some(json!({})),
                output: None,
            }),
        );

        let manager = HooksManager::new(vec![hook1, hook2], None);
        let result = manager.run(HookEvent::PreToolUse, &ctx).await;

        match result {
            Ok((outcomes, data)) => {
                assert_eq!(outcomes.len(), 2);
                match data {
                    HookContextData::Tool(t) => {
                        assert_eq!(t.input.and_then(|i| i.get("a").cloned()), Some(json!(2)));
                    }
                    _ => panic!("Expected Tool data"),
                }
            }
            Err(e) => panic!("Hook execution infrastructure failed: {e}"),
        }
    }

    #[tokio::test]
    async fn test_run_hooks_parallel_pre_prompt_concatenation() {
        let hook1 = Hook::from(HookDef {
            name: "p1".into(),
            event: HookEvent::PrePrompt,
            kind: HookType::Cmd,
            handler: r#"printf '{"system": "P1"}'"#.into(),
            matcher: None,
            on_failure: OnFailure::Abort,
            timeout_ms: None,
            scope: HookScope::Transform,
            strategy: ExecutionStrategy::Parallel,
            plugin_dir: None,
        });
        let hook2 = Hook::from(HookDef {
            name: "p2".into(),
            event: HookEvent::PrePrompt,
            kind: HookType::Cmd,
            handler: r#"printf '{"system": "P2"}'"#.into(),
            matcher: None,
            on_failure: OnFailure::Abort,
            timeout_ms: None,
            scope: HookScope::Transform,
            strategy: ExecutionStrategy::Parallel,
            plugin_dir: None,
        });

        let ctx = HookContext::new(
            HookEvent::PrePrompt,
            "/".into(),
            "123".into(),
            HookContextData::Prompt(PromptData {
                system: Some("S0".into()),
                query: Some("Q".into()),
            }),
        );

        let manager = HooksManager::new(vec![hook1, hook2], None);
        let result = manager.run(HookEvent::PrePrompt, &ctx).await;

        match result {
            Ok((outcomes, data)) => {
                assert_eq!(outcomes.len(), 2);
                match data {
                    HookContextData::Prompt(p) => {
                        assert_eq!(p.system.unwrap(), "S0\nP1\nP2");
                    }
                    _ => panic!("Expected Prompt data"),
                }
            }
            Err(e) => panic!("Hook execution infrastructure failed: {e}"),
        }
    }
}
