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

# Run a custom agent (YAML definition)
pie reviewer "check src/agent for me"
pie explore                      # opens the TUI running as that agent
```

### Interactive commands

| Input              | Action                               |
| ------------------ | ------------------------------------ |
| `<query>`          | Ask a question (auto-detects skills) |
| `/<skill> <query>` | Use a specific skill                 |
| `?`                | Show help                            |
| `Ctrl+C`           | Abort stream / quit                  |

## Custom agents (YAML)

Drop a `*.yaml` file into `~/.pie/agents/` (global) or `.pie/agents/`
(project-local, overrides global by name) and run it with
`pie <name> [query...]`. The first query token that matches an agent name
selects that agent — `pie reviewer check this` runs the `reviewer` agent
with `check this` as the query.

```yaml
# .pie/agents/reviewer.yaml
name: reviewer            # default: file name
description: Read-only code reviewer
model: deep               # a model tier from pie.toml, or a literal model id
max_steps: 50             # override the iteration limit
output_mode: md           # md | json | interactive

system_prompt: |          # the agent's persona; or use system_prompt_file: path
  You are a senior code reviewer. Be blunt and specific.

# Tools are opt-in: without a `plugins` list the agent has NO tools.
# Known names: fs, fs-readonly, shell, websearch, skills, agentsmd
plugins: [fs-readonly, shell]

skills_paths: ["~/src/my-skills"]   # extra skill directories (needs skills)

readonly: true            # demotes fs to fs-readonly (belt and suspenders)
sandbox:                  # full sandbox config, merged like pie.toml's
  allow_write: ["."]
grants: ["fs-read:/tmp"]  # pre-granted permissions
```

Markdown agents (`.pie/commands/*.md`) keep working; a YAML file with the
same name replaces its markdown twin. `pie skills` lists both.

## Configuration

Pie is configured via environment variables, a `pie.toml` file, or CLI flags.

### Environment Variables

The fastest way to get started is with environment variables:

```bash
export OPENAI_API_KEY="sk-..."
export OPENAI_MODEL="mlx-community/gemma-4-e4b-it-4bit"
export OPENAI_BASE_URL="http://localhost:1234/v1"

# For providers that support Anthropic-compatible endpoints (e.g. zai)
export ANTHROPIC_BASE_URL="https://api.z.ai/api/anthropic"
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

[provider.zai]
model = "glm-5.1"
base_url = "https://api.z.ai/api/paas/v4/"
anthropic_url = "https://api.z.ai/api/anthropic"
api_key = "..."
```
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
