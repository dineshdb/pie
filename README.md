# pie

A minimal AI coding agent written in Rust. Runs on Apple on-device models or any
OpenAI-compatible provider.

Pie is a CLI agent that answers questions, runs shell commands, and delegates to
skill-based subagents. It maintains persistent sessions per working directory
backed by SQLite.

## Usage

```bash
# One-shot query
pie "explain this function"

# Interactive mode
pie

# Continue the last session in this directory
pie -c

# JSON output (for scripting / piping)
pie --json "what is 2+2"

# Use a specific model/provider
pie --model gpt-4o --base-url https://api.openai.com/v1 --api-key sk-... "hello"

# Explicit skill invocation
pie -s "/search" "latest Rust features"
```

### JSON Output

Use `--json` for structured output suitable for piping into `jq` or other tools:

```bash
pie --json "list files in the current directory"
```

Returns:

```json
{
  "response": "Cargo.toml  README.md  src/  tests/",
  "session_id": "01960a1b-2c3d-7d4e-8f5a-6b7c8d9e0f1a",
  "model_used": "gpt-4o",
  "timestamp": "2026-04-10T12:34:56.789Z"
}
```

Fields:

| Field        | Description                           |
| ------------ | ------------------------------------- |
| `response`   | The agent's answer or command output  |
| `session_id` | UUID of the session (for `-c` resume) |
| `model_used` | Name of the model that was used       |
| `timestamp`  | UTC timestamp of the response         |

### Interactive commands

| Command            | Description                                   |
| ------------------ | --------------------------------------------- |
| `<query>`          | Ask a question (auto-detects relevant skills) |
| `/<skill> <query>` | Use a specific skill                          |
| `list-skills`      | Show available skills                         |
| `help`             | Show help text                                |
| `exit`             | Quit                                          |

## Skills

Skills are markdown files placed in `~/.pie/skills/<name>/SKILL.md` with YAML
frontmatter:

```markdown
---
name: search
description: Search the web for information
---

Skill instructions here...
```

Skills are auto-detected from queries mentioning `/<skill-name>` and injected
into the prompt. Skills can reference other skills, which are resolved
recursively.

## Sandboxing

Pie sandboxes **all** shell commands using
[sandbox-runtime](https://github.com/anthropic-experimental/sandbox-runtime)
(`srt`). This provides OS-level filesystem and network restrictions — no
containers required.

- **Required** — pie exits if `srt` is not on `PATH`
- Install: `npm install -g @anthropic-ai/sandbox-runtime`

### Sandbox configuration

Place `~/.pie/sandbox.json` to customise restrictions (uses defaults if absent):

```json
{
  "network": {
    "allowedDomains": ["github.com", "*.github.com", "npmjs.org"],
    "deniedDomains": []
  },
  "filesystem": {
    "denyRead": ["~/.ssh", "~/.gnupg"],
    "allowWrite": [".", "/tmp"],
    "denyWrite": [".env", ".env.local"]
  }
}
```

| Setting                  | Default              | Description                         |
| ------------------------ | -------------------- | ----------------------------------- |
| `network.allowedDomains` | package registries   | Domains the agent may access        |
| `network.deniedDomains`  | `[]`                 | Explicitly blocked domains          |
| `filesystem.denyRead`    | `~/.ssh`, `~/.gnupg` | Paths the agent cannot read         |
| `filesystem.allowWrite`  | `.`, `/tmp`          | Paths the agent may write to        |
| `filesystem.denyWrite`   | `.env`, `.env.local` | Paths blocked even if in allowWrite |

## Configuration

- **Config directory**: `~/.pie/`
- **Sandbox config**: `~/.pie/sandbox.json`
- **System prompt override**: `~/.pie/SYSTEM_PROMPT.md` (MiniJinja template)
- **Subagent prompt override**: `~/.pie/SUBAGENT_PROMPT.md`
- **Project instructions**: Place `AGENTS.md` in your project root (or any
  parent directory)
- **Global instructions**: `~/.pie/AGENTS.md`

## Comparison

|                     | **Pie**                | **Claude Code**            | **Codex CLI**    | **Pi**                |
| ------------------- | ---------------------- | -------------------------- | ---------------- | --------------------- |
| Language            | Rust                   | TypeScript                 | Rust             | Python                |
| Runtime             | Single binary          | Node.js                    | Single binary    | Python + venv         |
| Provider            | Any OpenAI-compatible  | Anthropic only             | OpenAI only      | Any OpenAI-compatible |
| Apple on-device     | Yes                    | No                         | No               | No                    |
| Session persistence | SQLite per directory   | JSON per project           | None             | Per conversation      |
| Skill system        | Markdown + frontmatter | CLAUDE.md + slash commands | None             | Markdown skills       |
| Subagents           | Skill-based delegation | Task agents                | None             | Skill-based           |
| Sandboxing          | srt (OS-level)         | srt (OS-level)             | Docker container | None                  |
| JSON output         | `--json`               | `--output-format json`     | `--format json`  | None                  |
| Interactive mode    | REPL                   | REPL                       | No               | REPL                  |
| Streaming           | Yes                    | Yes                        | Yes              | Yes                   |
| License             | MIT                    | MIT (OSS) / prop (hosted)  | Apache-2.0       | MIT                   |

## Architecture

The agent uses [aisdk](https://github.com/qretaio/aisdk) for provider-agnostic
LLM calls with streaming, tool use, and step-limited execution loops.

## Build

```bash
cargo build --release
```

## Test

```bash
# Unit tests
cargo test

# Integration tests (offline — skips model-dependent tests)
uv run scripts/test.py offline

# Integration tests (requires model server on localhost:8000)
uv run scripts/test.py
```

## License

MIT
