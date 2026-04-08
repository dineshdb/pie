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

# Use a specific model/provider
pie --model gpt-4o --base-url https://api.openai.com/v1 --api-key sk-... "hello"

# Explicit skill invocation
pie -s "/search" "latest Rust features"
```

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

## Configuration

- **Config directory**: `~/.pie/`
- **System prompt override**: `~/.pie/SYSTEM_PROMPT.md` (MiniJinja template)
- **Subagent prompt override**: `~/.pie/SUBAGENT_PROMPT.md`
- **Project instructions**: Place `AGENTS.md` in your project root (or any
  parent directory)
- **Global instructions**: `~/.pie/AGENTS.md`

## Architecture

The agent uses [aisdk](https://github.com/qretaio/aisdk) for provider-agnostic
LLM calls with streaming, tool use, and step-limited execution loops.

## Build

```bash
cargo build --release
```

## License

MIT
