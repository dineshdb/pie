# TASKS

- [ ] Add support for markdown rendering in the interactive tui
- [ ] Improve internal skills
- [ ] dogfooding
  - [ ] for reviews
  - [ ] for implementing features
- [ ] Implement File Registry: Track read files, line counts, and key symbols to prevent redundant context loading.
- [ ] Implement Adaptive Compaction: Replace large, old tool outputs (e.g., compiler logs) with bounded summaries to manage context bloat.
- [ ] Implement Doom Loop Detector: Track repeated execution of the same commands (e.g., , ) without code edits to detect infinite loops.
- [ ] Refine Truncation Logic: Implement per-command output bounding (e.g., 24KB for , 16KB for compiler errors) instead of relying on generic limits.
- [ ] Integrate Startup Sequence: Implement the sequence to load  and generate a bounded file tree snapshot on session start.
