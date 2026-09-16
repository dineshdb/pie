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

## A2A server (agent-to-agent over HTTP)

`pie server` runs an HTTP daemon that exposes pie to other agents over the
[Agent2Agent protocol](https://a2a-protocol.org) (v1.0, JSON-RPC over
streamable HTTP) — the door for a main agent that delegates work to pie as
needed, or for a UI driving pie sessions. The daemon is pie's assembly of
the [`a2acp`](https://github.com/qretaio/a2acp) gateway: pie is hosted as
the gateway's in-process agent (the same engine the TUI drives), and the
wire behavior — agent card, task lifecycle, the `INPUT_REQUIRED`
permission flow — is the crate's. One daemon serves many conversations,
reusing its database pool and provider config.

```bash
# Run in the foreground (bind defaults to 127.0.0.1:8629)
pie server

# Or install it as a login service that restarts on failure.
# `--host` lists the hostname remote clients will use (required: the
# server answers 403 to any non-loopback Host header that is not
# allowlisted, a DNS-rebinding guard).
pie server install --bind 127.0.0.1:8629 --host citadel.lvh.me
```

**Auth.** Static bearer tokens are gone (`pie server token` was removed):
the gateway authenticates the a2acp way — either OpenID Connect
(`[server] openid_connect_url` in pie.toml; the agent card then declares
the standard `openIdConnect` scheme and every RPC must carry a
provider-issued bearer JWT), or no auth in the application with the bind
kept loopback and exposed through `tailscale serve` (the tailnet is the
authentication). A non-loopback bind without OIDC is refused, and so is a
leftover `[server] api_key` — delete it.

Discovery is the agent card (unauthenticated; it declares the auth scheme
when one is configured):

```
GET http://127.0.0.1:8629/.well-known/agent-card.json
POST http://127.0.0.1:8629/a2a        (JSON-RPC 2.0)
```

Send `SendStreamingMessage` and read the SSE response: a `Task` frame
first, then `artifactUpdate` per token delta and `statusUpdate` per tool
call, ending with a final status. `SendMessage` is the blocking fallback
(one request, final task); `SubscribeToTask` reattaches after a dropped
connection; `CancelTask` aborts the in-flight turn; `GetTask`/`ListTasks`
inspect and enumerate; `DeleteTask` removes a task or a whole
conversation. Tasks and transcripts are durable in the gateway's own
SQLite store (`~/.config/a2acp/a2a.sqlite3`); pie's database keeps
sessions, usage, and cron.

**External agents.** A2A clients select the agent by `metadata.agent`
(default `pie`). Additional ACP-speaking agents can be served alongside
pie — the server counterpart of the interactive `--acp-agent` flag:

```toml
[server.agents.opencode]
command = "opencode"
args = ["acp"]
```

Each becomes a skill on the agent card; the gateway spawns one process
per session. `[server] url` overrides the public URL baked into the card
(set it to the tailscale HTTPS URL when serving through `tailscale
serve`).

**Known gaps vs the old pie-native server** (tracked as `TODO(a2acp)` in
the crate): no push-notification webhooks (`pushNotifications: false` on
the card), no `GetExtendedAgentCard` with per-persona skills, and the
task model is one task per turn (a finished turn `COMPLETES` its task;
continuation is a new task on the same `contextId`) instead of the old
conversation-is-one-task model.

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

##### OAuth servers

Servers behind OAuth 2.1 take an `[mcp.<name>.auth]` section instead of an
`api_key`. Run `pie mcp login <name>` once: pie performs the MCP-spec
browser flow — metadata discovery, dynamic client registration (or your
pre-registered credentials), authorization code + PKCE — and stores the
tokens in `~/.pie/pie.db`. Runs then authorize from the store and refresh
automatically; with no stored token, strict selections fail and
best-effort runs skip the server with a `pie mcp login` hint.

```toml
[mcp.linear]
url = "https://mcp.linear.app/mcp"

[mcp.linear.auth]
# omit client_id on servers supporting dynamic client registration
client_id = "pie-client"
client_secret = "linear_secret"   # optional; pairs with client_id
# scopes = ["read", "write"]      # empty adopts what the server advertises
# redirect_port = 8123            # only if the server requires a fixed redirect URI

[secrets]
linear_secret = "..."
```

`pie mcp logout <name>` forgets the stored tokens. `api_key` and `auth`
are mutually exclusive — configure one, not both.

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
