//! One in-flight turn per session, across every door into the engine.
//!
//! The daemon serves pie turns over several protocols at once (MCP tools,
//! A2A tasks); chats are sequential, so a second turn on the *same* session
//! must be refused while one is running — no matter which door it arrives
//! through. Sessions in different workspaces are independent and never
//! contend.
//!
//! The gate maps session id → door label. Acquiring returns a
//! [`TurnGuard`] whose `Drop` releases the slot, so a panicked or dropped
//! turn task cannot leak a stuck session.

use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex, MutexGuard, PoisonError};

/// Shared turn-exclusivity registry. Cloneable handle onto one map.
#[derive(Clone, Default)]
pub struct TurnGate {
    running: Arc<StdMutex<HashMap<String, String>>>,
}

impl TurnGate {
    /// Claim the turn slot for `session`, naming the door (`"mcp:<task>"`,
    /// `"a2a:<task>"`) that now owns it. Fails with the current owner's
    /// label when a turn is already in flight.
    pub fn try_acquire(&self, session: &str, door: &str) -> Result<TurnGuard, String> {
        let mut running = self.lock();
        if let Some(owner) = running.get(session) {
            return Err(owner.clone());
        }
        running.insert(session.to_string(), door.to_string());
        Ok(TurnGuard {
            gate: Arc::clone(&self.running),
            session: session.to_string(),
        })
    }

    /// Whether a turn is currently running for `session`.
    pub fn is_running(&self, session: &str) -> bool {
        self.lock().contains_key(session)
    }

    /// Whether no turn is running at all (test and observability aid).
    pub fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<String, String>> {
        self.running.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Released automatically on drop.
pub struct TurnGuard {
    gate: Arc<StdMutex<HashMap<String, String>>>,
    session: String,
}

impl Drop for TurnGuard {
    fn drop(&mut self) {
        self.gate
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.session);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn guard_release_frees_the_slot() {
        let gate = TurnGate::default();
        let owner = gate.try_acquire("s1", "mcp:t1").unwrap();
        match gate.try_acquire("s1", "a2a:t2") {
            Err(owner) => assert_eq!(owner, "mcp:t1"),
            Ok(_) => panic!("second acquire on a busy session must fail"),
        }
        drop(owner);
        assert!(gate.try_acquire("s1", "a2a:t2").is_ok());
    }

    #[test]
    fn different_sessions_never_contend() {
        let gate = TurnGate::default();
        let _a = gate.try_acquire("s1", "mcp:t1").unwrap();
        assert!(gate.try_acquire("s2", "a2a:t2").is_ok());
        assert!(!gate.is_running("s3"));
        assert!(gate.is_running("s1"));
    }
}
