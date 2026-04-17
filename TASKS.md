# TASKS

- [x] Add support for markdown rendering in the interactive tui
- [-] Add .pie/agents/ support that defines what each agent does. This splits
  skills as composable and agents as runnable.
- [ ] Restrict /plan mode to read only, while only allowing it to write plan files.
- [ ] Batching the tool calls for faster iterations.
- [ ] TUI
  - [ ] History
- [ ] Implement File Registry: Track read files, line counts, and key symbols to
      prevent redundant context loading.
- [ ] Implement Adaptive Compaction: Replace large, old tool outputs (e.g.,
      compiler logs) with bounded summaries to manage context bloat.
- [ ] Implement Doom Loop Detector: Track repeated execution of the same
      commands without code edits to detect infinite loops.
- [ ] Refine Truncation Logic: Implement per-command output bounding (e.g., 24KB
      for , 16KB for compiler errors) instead of relying on generic limits.
