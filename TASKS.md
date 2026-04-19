# TASKS

- [x] Add support for markdown rendering in the interactive tui
- [x] Add .pie/agents/ support that defines what each agent does. This splits
      skills as composable and agents as runnable.
- [ ] Restrict /plan mode to read only, while only allowing it to write plan
      files.
- [x] Batching the tool calls for faster iterations.
- [x] Unless --md or --json is mentioned, pie starts in interactive mode.
- [ ] So progress updates as model continues working.
- [ ] Show tool calls in message history
- [x] rendered markdown is not selectable for copying
- [ ] Parse input into Query which extracts skills and agent names from it.
- [ ] /model for model info and switching models
- [ ] Implement Adaptive Compaction: Replace large, old tool outputs (e.g.,
      compiler logs) with bounded summaries to manage context bloat.
- [ ] Implement Doom Loop Detector: Track repeated execution of the same
      commands without code edits to detect infinite loops.
- [ ] Refine Truncation Logic: Implement per-command output bounding (e.g., 24KB
      for , 16KB for compiler errors) instead of relying on generic limits.
