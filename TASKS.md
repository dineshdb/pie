# TASKS

- [ ] Implement Adaptive Compaction: Replace large, old tool outputs (e.g.,
      compiler logs) with bounded summaries to manage context bloat.
- [x] Implement Doom Loop Detector: Track repeated execution of the same
      commands without code edits to detect infinite loops.
- [x] Memory Layer (Hierarchical instruction loading)
- [x] Plugin system for defining capability.
- [ ] LLM router for cost based request routing
- [ ] Persistent subagents
- [ ] Highlight the skill and agent names in the input box.
- [ ] Add hook for after conversation completion
  - [ ] Use it to run completion checks and nudge the system to get to back to work.
  - [ ] System Notifications
- [ ] Define tools such that they can check and run automatically at different points.
- [ ] Tool Call Serialization via json field.
- [ ] While persisting tool calls, let's replace previous tool call entry in database with a one with content.
- [ ] Let's reconstruct the history such that it is no different from it being constructed while running the agent. Making them identical. That way, don't need to inject history into the system prompt.
