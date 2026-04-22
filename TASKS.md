# TASKS

- [ ] Security
  - [ ] Restrict /plan mode to read only, while only allowing it to write plan
        files.
- [x] Unless --md or --json is mentioned, pie starts in interactive mode.
- [ ] Skills
  - [ ] Add support for executing reference scripts
- [ ] Tasks
  - [ ] Add support for handling tasks.
  - [ ] Show progress update as the indivitual task progresses
- [ ] Parse input into Query which extracts skills and agent names from it.
- [ ] /model for model info and switching models
- [ ] Implement Adaptive Compaction: Replace large, old tool outputs (e.g.,
      compiler logs) with bounded summaries to manage context bloat.
- [ ] Implement Doom Loop Detector: Track repeated execution of the same
      commands without code edits to detect infinite loops.
