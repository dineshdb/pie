# pie

A fast, minimal AI coding agent in Rust. Any OpenAI-compatible provider,
persistent sessions, skill-based subagents, and sandboxed shell execution.

> **Disclaimer:** This project is actively developed. While it supports any
> OpenAI-compatible API, **not all models have been thoroughly tested**.
> `mlx-community/gemma-4-e4b-it-4bit` (Gemma 4) is used as the primary model for
> development and testing.

## Quick start

```bash
# Interactive mode — just start talking
pie

# resume your last conversation in this directory
pie -r # or pie --resume

# Pipe a question
echo "what does src/main.rs do?" | pie --md

# Use a specific model
pie -m mlx-community/gemma-4-e4b-it-4bit
```

## Features

- **Persistent sessions** — conversations saved per directory, resume with
  `pie -r`
- **Any provider** — works with OpenAI, Anthropic, Groq, Ollama, or any
  OpenAI-compatible API
- **Skills & subagents** — markdown-based skills from
  [agentskills.io](https://agentskills.io), auto-loaded from queries
- **Native Sandboxing** — shell commands run with built-in OS isolation
  (sandbox-exec on macOS, bubblewrap on Linux)
- **Streaming TUI** — real-time tool calls, markdown rendering, command history
- **Scriptable** — `--json` and `--md` flags for single-shot mode

## Usage

```bash
# Interactive (default)
pie

# Resume last session
pie -r

# Single-shot output
pie --md "explain this function"
pie --json "list files"   # pipe into jq, etc.

# Use a specific skill
pie "/explore summarize this repo"
```

### Interactive commands

| Input              | Action                               |
| ------------------ | ------------------------------------ |
| `<query>`          | Ask a question (auto-detects skills) |
| `/<skill> <query>` | Use a specific skill                 |
| `?`                | Show help                            |
| `Ctrl+C`           | Abort stream / quit                  |

## Configuration

Pie is configured via environment variables, a `pie.toml` file, or CLI flags.

### Environment Variables

The fastest way to get started is with environment variables:

```bash
export OPENAI_API_KEY="sk-..."
export OPENAI_MODEL="mlx-community/gemma-4-e4b-it-4bit"
export OPENAI_BASE_URL="http://localhost:1234/v1"
```

### `pie.toml`

For managing multiple providers or project-specific settings, use `pie.toml`.
Pie searches for this file in:

1. `~/.pie/pie.toml` (Global configuration)
2. `./.pie/pie.toml` (Project-specific configuration)

For a full list of configuration options, see
[.pie/pie.toml.example](.pie/pie.toml.example).

#### Example `pie.toml`

```toml
default_provider = "local"

[provider.local]
model = "mlx-community/gemma-4-e4b-it-4bit"
base_url = "http://localhost:1234/v1"
api_key = "sk-..."

[provider.ollama]
model = "llama3"
base_url = "http://localhost:11434/v1"

[agent]
max_steps = 25 # Max tool-call iterations per query
```

To use a specific provider from your config:

```bash
pie -p ollama "how are you?"
```

### CLI Flags

| Flag               | Description                   |
| ------------------ | ----------------------------- |
| `-m`, `--model`    | Model name                    |
| `--base-url`       | API base URL                  |
| `--api-key`        | API key                       |
| `-p`, `--provider` | Config provider name          |
| `-r`, `--resume`   | Continue last session         |
| `--md`             | Markdown output (single-shot) |
| `--json`           | JSON output (single-shot)     |
| `-d`, `--debug`    | Debug logging                 |

### Advanced Configuration

- **Sandbox:** Configure restrictions in `pie.toml` under `[sandbox]`.
- **Skills:** Add custom skills to `.pie/skills/<name>/SKILL.md`.
- **Instructions:** Add project-level instructions to `AGENTS.md`.

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
