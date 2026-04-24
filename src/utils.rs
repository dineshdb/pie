use retry::delay::Fibonacci;
use retry::{OperationResult, retry};
use std::ops::ControlFlow;
use std::time::{Duration, Instant};

pub fn load_file(path: impl AsRef<std::path::Path>) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

/// Merge `incoming` items into `items` by name, overriding duplicates.
pub fn merge_by_name<T, F>(
    items: &mut Vec<T>,
    names: &mut std::collections::HashSet<String>,
    incoming: Vec<T>,
    get_name: F,
) where
    F: Fn(&T) -> &str,
{
    for item in incoming {
        let name = get_name(&item);
        if names.contains(name) {
            if let Some(existing) = items.iter_mut().find(|i| get_name(i) == name) {
                *existing = item;
            }
        } else {
            names.insert(name.to_string());
            items.push(item);
        }
    }
}

/// Check if an HTTP status code is retriable (429, 500, 502, 503, 504).
pub fn is_retriable_status(status: u16) -> bool {
    matches!(status, 429 | 500 | 502 | 503 | 504)
}

/// Check if an error is retriable (e.g., network timeout, 429, 5xx).
pub fn is_retriable_error(err: &impl std::fmt::Display) -> bool {
    let msg = err.to_string().to_lowercase();

    // Check for explicit status codes in the error message
    for code in ["429", "500", "502", "503", "504"] {
        if msg.contains(code) {
            return true;
        }
    }

    msg.contains("timeout")
        || msg.contains("connection reset")
        || msg.contains("broken pipe")
        || msg.contains("connection refused")
        || msg.contains("dns error")
}

/// Execute an operation with 8-minute exponential retry logic using the `retry` crate.
/// Wraps the synchronous retry loop in `spawn_blocking` to avoid blocking the async executor.
pub async fn execute_with_retry<T, F, Fut>(label: &str, mut make_op: F) -> anyhow::Result<T>
where
    F: FnMut() -> Fut + Send + 'static,
    Fut: Future<Output = anyhow::Result<T>> + Send,
    T: Send + 'static,
{
    let handle = tokio::runtime::Handle::current();
    let label = label.to_string();

    tokio::task::spawn_blocking(move || {
        let start = Instant::now();
        let max_duration = Duration::from_mins(8);
        let strategy = Fibonacci::from_millis(2000).take_while(|_| start.elapsed() < max_duration);

        retry(strategy, || match handle.block_on(make_op()) {
            Ok(r) => OperationResult::Ok(r),
            Err(e) if is_retriable_error(&e) => {
                if start.elapsed() >= max_duration {
                    OperationResult::Err(e)
                } else {
                    tracing::warn!(
                        operation = %label,
                        error = %e,
                        elapsed = ?start.elapsed(),
                        "retrying"
                    );
                    OperationResult::Retry(e)
                }
            }
            Err(e) => OperationResult::Err(e),
        })
        .map_err(|e| e.error)
    })
    .await?
}

use std::future::Future;
/// Walk from cwd upward, calling `check` on each directory.
/// Stops at home directory, filesystem root, or after 32 levels.
/// The closure returns `Break(Some(T))` when found, `Break(None)` to stop, `Continue(())` to keep going.
fn walk_upward<T>(mut check: impl FnMut(&std::path::Path) -> ControlFlow<Option<T>>) -> Option<T> {
    let home = dirs::home_dir();
    let mut dir = std::env::current_dir().ok()?;
    for _ in 0..32 {
        match check(&dir) {
            ControlFlow::Break(value) => return value,
            ControlFlow::Continue(()) => {}
        }
        if home.as_ref().is_some_and(|h| dir == *h) || !dir.pop() {
            return None;
        }
    }
    None
}

/// Walk from `PWD` upward to find a file, stopping at the git repo root or home directory.
pub fn find_upward_in_repo(name: &str) -> Option<String> {
    walk_upward(|dir| {
        if let Some(content) = load_file(dir.join(name)) {
            return ControlFlow::Break(Some(content));
        }
        if dir.join(".git").exists() {
            return ControlFlow::Break(None);
        }
        ControlFlow::Continue(())
    })
}

/// Find the git repo root by walking up from cwd looking for `.git`.
/// Stops at the user's home directory to avoid scanning system paths.
pub fn git_repo_root() -> Option<String> {
    walk_upward(|dir| {
        if dir.join(".git").exists() {
            ControlFlow::Break(Some(dir.display().to_string()))
        } else {
            ControlFlow::Continue(())
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_by_name_adds_new_items() {
        let mut items = vec!["a".to_string()];
        let mut names = std::collections::HashSet::from(["a".to_string()]);
        merge_by_name(&mut items, &mut names, vec!["b".to_string()], |s| s);
        assert_eq!(items, vec!["a", "b"]);
    }

    #[test]
    fn merge_by_name_overrides_duplicates_in_place() {
        // Use (name, version) pairs so we can observe the replacement
        let mut items = vec![("a", "v1"), ("b", "v1")];
        let mut names = std::collections::HashSet::from(["a".to_string(), "b".to_string()]);
        merge_by_name(
            &mut items,
            &mut names,
            vec![("a", "v2")],
            |p: &(&str, &str)| p.0,
        );
        assert_eq!(items.len(), 2, "override should not increase length");
        assert_eq!(
            items.first(),
            Some(&("a", "v2")),
            "existing item should be replaced"
        );
        assert_eq!(items.get(1), Some(&("b", "v1")), "unrelated item unchanged");
    }

    #[test]
    fn merge_by_name_mixed_add_and_override() {
        let mut items = vec!["a".to_string()];
        let mut names = std::collections::HashSet::from(["a".to_string()]);
        merge_by_name(
            &mut items,
            &mut names,
            vec!["a".to_string(), "c".to_string()],
            |s| s,
        );
        assert_eq!(items, vec!["a", "c"]);
    }

    #[test]
    fn merge_by_name_empty_incoming() {
        let mut items = vec!["a".to_string()];
        let mut names = std::collections::HashSet::from(["a".to_string()]);
        merge_by_name::<String, _>(&mut items, &mut names, vec![], |s| s);
        assert_eq!(items, vec!["a"]);
    }

    #[test]
    fn test_is_retriable_status() {
        assert!(is_retriable_status(429));
        assert!(is_retriable_status(500));
        assert!(is_retriable_status(503));
        assert!(!is_retriable_status(401));
        assert!(!is_retriable_status(404));
        assert!(!is_retriable_status(200));
    }

    #[test]
    fn test_is_retriable_error() {
        assert!(is_retriable_error(&"timeout".to_string()));
        assert!(is_retriable_error(&"429 Too Many Requests".to_string()));
        assert!(is_retriable_error(&"502 Bad Gateway".to_string()));
        assert!(is_retriable_error(&"Connection reset by peer".to_string()));
        assert!(is_retriable_error(&"dns error".to_string()));
        assert!(!is_retriable_error(&"401 Unauthorized".to_string()));
        assert!(!is_retriable_error(&"404 Not Found".to_string()));
    }
}
