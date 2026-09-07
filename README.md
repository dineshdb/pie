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

# Run a custom agent (markdown definition)
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

## Editor integration (ACP)

`pie acp` serves the [Agent Client Protocol](https://agentclientprotocol.com)
over stdio, so any ACP client (Zed, and other editors with ACP support) can
use pie as its coding agent.

```json
// Zed: ~/.config/zed/settings.json
{
  "agent_servers": {
    "pie": {
      "type": "custom",
      "command": "pie",
      "args": ["acp"]
    }
  }
}
```

What the client gets:

- **Sessions** — `session/new` creates a persistent pie session; `session/load`
  resumes one and replays the history. Session ids are pie session ids, so
  `pie -r` on the same directory picks up where the editor left off.
- **Workspace trust** — the `cwd` (and `additionalDirectories`) the client
  sends is granted read+write in the sandbox for that session, and commands
  run with the session directory as their working directory. `~/.ssh`,
  `.env` and the other `deny_*` rules still apply on top.
- **Modes** — pie's plan/build/debug/test/review/architect modes appear as
  the session's mode selector; `session/set_mode` switches between them.
- **Approval before anything changes** — Write, Edit and Bash each wait for a
  `session/request_permission` answer (allow once / always for that tool this
  session / reject). Shell is gated too: the sandbox draws the boundary, but
  inside a writable workspace `printf 'x' > f` is an edit like any other, and
  a client that approves edits should not be walked around. Reads (Read, Ls,
  Glob, Grep) never ask.

## Custom agents (markdown)

Agents are markdown files: frontmatter for configuration, body is the
agent's system prompt. Drop a `*.md` file into `~/.pie/agents/` (global)
or `.pie/agents/` (project-local, overrides global by name) and run it
with `pie <name> [query...]` — a first query token matching an agent name
selects it.

```markdown
---
name: reviewer            # default: file name
description: Read-only code reviewer
model: deep               # a model tier from pie.toml, or a literal model id
max_steps: 50             # cap the tool-call iterations (unset: unbounded)
output_mode: md           # md | json | interactive
temperature: 0.3

# Tools are opt-in for agents/ drops: without a `plugins` list the agent
# has NO tools. Known names: fs, fs-readonly, shell, websearch, skills,
# agentsmd, mcp (all configured servers) or mcp:<server> for specific
# ones — unknown names fail the run.
plugins: [fs-readonly, shell]

skills_paths: ["~/src/my-skills"]   # extra skill directories (needs skills)
readonly: true            # demotes fs to fs-readonly (belt and suspenders)
sandbox:                  # full sandbox config, merged like pie.toml's
  allow_write: ["."]
grants: ["fs-read:/tmp"]  # pre-granted permissions
---

You are a senior code reviewer. Be blunt and specific.
(The rest of this file is the agent's system prompt.)
```

Built-in commands (`.pie/commands/*.md`, embedded and global/local
`commands/` dirs) keep working unchanged and keep the full default tool
set — a `plugins:` list there is also honored if present. An `agents/`
file with the same name overrides its `commands/` twin. `pie skills`
lists everything.

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

#### MCP servers

HTTP-based [MCP](https://modelcontextprotocol.io/) servers are configured
under `[mcp.<name>]`. Default runs (and legacy `commands/` agents) connect
to every configured server best-effort — a server that is down costs a
warning in the session log, never the run. An agent with an explicit
`plugins:` list gets exactly what it names: `plugins: [mcp]` (every server,
fail-loud) or `plugins: ["mcp:<name>"]` (specific ones, fail-loud). Their
tools appear as `<name>__<tool>`.

```toml
[mcp.deepwiki]
url = "https://mcp.deepwiki.com/mcp"

[mcp.context7]
url = "https://mcp.context7.com/mcp"

[mcp.context7.headers]
# a value matching a [secrets] key is replaced by that secret at load time
CONTEXT7_API_KEY = "context7_key"

# GitHub's remote MCP (needs a PAT in [secrets] as github_pat):
# [mcp.github]
# url = "https://api.githubcopilot.com/mcp/"
# [mcp.github.headers]
# Authorization = "github_pat"

[secrets]
context7_key = "..."
```

#### Usage & cost tracking

Every interaction records its LLM usage — request count, prompt/completion
tokens, cached tokens and reasoning tokens — and prints a summary line when
the run finishes (non-interactive mode):

```
· done in 6s · 4.6k tokens · 98% cached · 1 request
```

`--json` output carries the same stats in a `usage` object (`cache_rate`
is the cached fraction of prompt tokens; `cost_usd` is `null` without
configured pricing).

For cost accounting, configure per-model rates in USD per million tokens
under `[pricing.<model-id>]` (exact model id match):

```toml
[pricing."glm-5.1"]
input = 0.6        # uncached input tokens
cached_input = 0.1 # cache hits; defaults to `input` when omitted
output = 2.2       # output tokens
```

All runs are persisted to the `llm_usage` table in `~/.pie/pie.db` for
bookkeeping. `pie usage` aggregates spend per model (defaults to the last
30 days; `--days 0` is all time, `--json=` emits machine-readable output):

```bash
pie usage
pie usage --days 7
```

```
LLM usage, last 30 days

model                req  prompt  compl   total  cached  cache  cost
@z-ai/glm-5.3-flash   18  126.8k   3.2k  130.0k  114.6k    90%     —
────────────────────────────────────────────────────────────────────
total                 18  126.8k   3.2k  130.0k  114.6k    90%     —
```

Raw queries work too:

```bash
sqlite3 ~/.pie/pie.db "SELECT model, SUM(total_tokens), SUM(cost_usd) \
  FROM llm_usage GROUP BY model"
```

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
