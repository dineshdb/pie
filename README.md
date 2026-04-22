# pie

A fast, minimal AI coding agent in Rust. Any OpenAI-compatible provider, persistent sessions, skill-based subagents, and sandboxed shell execution.

## Quick start

```bash
# Interactive mode — just start talking
pie

# Continue your last conversation in this directory
pie -c

# Pipe a question
echo "what does src/main.rs do?" | pie --md

# Use a specific model
pie -m claude-sonnet-4-20250514
```

## Features

- **Persistent sessions** — conversations saved per directory, resume with `pie -c`
- **Any provider** — works with OpenAI, Anthropic, Groq, Ollama, or any OpenAI-compatible API
- **Skills & subagents** — markdown-based skills from [agentskills.io](https://agentskills.io), auto-loaded from queries
- **Sandboxed commands** — all shell commands run through [srt](https://github.com/anthropic-experimental/sandbox-runtime) (OS-level isolation, no containers)
- **Streaming TUI** — real-time tool calls, markdown rendering, command history
- **Scriptable** — `--json` and `--md` flags for single-shot mode

## Usage

```bash
# Interactive (default)
pie

# Continue last session
pie -c

# Single-shot output
pie --md "explain this function"
pie --json "list files"   # pipe into jq, etc.

# Use a specific skill
pie "/explore summarize this repo"
```

### Interactive commands

| Input              | Action                                  |
| ------------------ | --------------------------------------- |
| `<query>`          | Ask a question (auto-detects skills)    |
| `/<skill> <query>` | Use a specific skill                    |
| `?`                | Show help                               |
| `Ctrl+C`           | Abort stream / quit                     |

## Configuration

```bash
# .pie/sandbox.json — sandbox restrictions
# .pie/skills/<name>/SKILL.md — custom skills
# .pie/agents/<name>.md — custom subagents
# AGENTS.md — project-level instructions
```

| Flag          | Description                     |
| ------------- | ------------------------------- |
| `-m`          | Model name                      |
| `--base-url`  | API base URL                    |
| `--api-key`   | API key                         |
| `-c`          | Continue last session           |
| `--md`        | Markdown output (single-shot)   |
| `--json`      | JSON output (single-shot)       |
| `-d`          | Debug logging                   |

## Install

### Homebrew (macOS / Linux)

```bash
brew tap dineshdb/pie https://github.com/dineshdb/pie
brew install dineshdb/pie/pie
```

### From source

```bash
cargo build --release
```

## License

MIT
