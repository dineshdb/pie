# TASKS

- [ ] Security
  - [ ] Restrict /plan mode to read only, while only allowing it to write plan
        files.
- [ ] Tasks
  - [ ] Add support for handling tasks.
  - [ ] Show progress update as the indivitual task progresses
- [ ] Implement Adaptive Compaction: Replace large, old tool outputs (e.g.,
      compiler logs) with bounded summaries to manage context bloat.
- [ ] Implement Doom Loop Detector: Track repeated execution of the same
      commands without code edits to detect infinite loops.
- [ ] Bugfix: Continue on network error. Retry with full context.
