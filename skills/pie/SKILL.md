---
name: pie
description: Invoke the pie agent CLI — a non-interactive, one-shot AI assistant for research, code review, codebase exploration, docs lookup, planning, and code simplification. Use to offload work from the main Claude session and reduce token consumption.
---

# Pie Agent

Pie is a CLI-based AI agent that runs **non-interactive, one-shot queries**. It
has no conversation history — every query must be fully self-contained. Use it
to offload analysis, research, and exploration from the main session.

## Requirements

- `pie` must be installed and on `PATH` — verify with `which pie`
- If missing: install from <https://github.com/nickgermain/pie> or check project
  docs

## When to Use

- **Offload research** — web search, documentation lookup, framework comparisons
- **Code review** — correctness, security, performance analysis
- **Codebase exploration** — structure, dependencies, patterns, recent activity
- **Implementation planning** — context-gathered plans with file-level detail
- **Code simplification** — find duplication, dead code, over-abstraction
- **Documentation lookup** — fetch library docs and code examples via Context7
- **General queries** — any well-scoped question that doesn't need the main
  session's context

When NOT to use: trivial lookups (use Grep/Read), file edits (pie is read-only
analysis), multi-turn conversations.

## Rules

### 1. Always use `--md`

Every invocation must include `--md` for markdown-formatted output:

```bash
pie --md "YOUR QUERY HERE"
```

### 2. Write detailed, self-contained queries

Pie has no conversation context. Every query must include all necessary
information:

```bash
# BAD — vague, no context
pie --md "fix the bug"

# GOOD — specific, self-contained
pie --md "Review the function parse_config in src/config.rs for:
1. Missing files → should return ConfigError::NotFound
2. Invalid TOML → should return ConfigError::ParseFailed
3. Empty required fields → should return ConfigError::MissingField
Check for: unwrap() calls, missing error variants, inconsistent error messages."
```

### 3. List capabilities first

Before delegating, check what's available — capabilities may change:

```bash
pie --list-skills
```

### 4. Target the right agent

| Agent        | Trigger                                   | Best For                                 |
| ------------ | ----------------------------------------- | ---------------------------------------- |
| **docs**     | "Fetch documentation for..."              | Library/framework API docs               |
| **explore**  | "Explore the codebase at..."              | Structure, dependencies, patterns        |
| **plan**     | "Create an implementation plan for..."    | Implementation planning with context     |
| **review**   | "Review the code in [file] for..."        | Correctness, security, performance       |
| **simplify** | "Find simplification opportunities in..." | Duplication, dead code, over-abstraction |

### 5. Validate output

After pie returns:

1. Read the full output — don't truncate
2. Check for errors — connection failures, model errors, empty responses
3. Incorporate useful results into the main session

## Workflow

```
1. pie --list-skills          → check available agents/skills
2. pie --md "detailed query"  → run self-contained query
3. Validate output            → check for errors, extract results
4. Use results                → incorporate into main session work
```

## Command Reference

```bash
pie --list-skills               # List available skills and agents
pie --md "query"                # Markdown output (ALWAYS use this)
pie --json "query"              # JSON output (for programmatic parsing)
pie --md -m MODEL "query"      # Specify model
pie -d --md "query"            # Debug mode
```

## Example Queries

### Code Review

```bash
pie --md "Review src/api/handlers.rs for correctness, security, and performance.
Check for: unwrap()/expect() that could panic, SQL injection, unnecessary allocations,
missing error handling, Rust idiom violations.
Output as: ## Issues (HIGH/MEDIUM/LOW) then ## Suggestions"
```

### Documentation Lookup

```bash
pie --md "Fetch the latest docs for the Rust itertools crate.
Focus on: flat_map, peekable, merge. Include code examples."
```

### Codebase Exploration

```bash
pie --md "Explore the codebase at . and provide:
1. Directory structure overview
2. Main entry points and key modules
3. Dependency graph
4. Test coverage locations
Output as structured markdown."
```

### Implementation Planning

```bash
pie --md "Create an implementation plan for adding SQLite support.
Requirements: rusqlite with migrations, store user preferences, atomic transactions.
Current state: uses YAML files for config (see .config/qai/config.yaml).
Output: step-by-step plan with files to create/modify and verification commands."
```

### Code Simplification

```bash
pie --md "Find simplification opportunities in src/services/.
Look for: duplicated logic, dead code, over-abstracted traits, missed stdlib reuse.
Output: ## Findings with file:line references and concrete simplifications."
```

## Anti-Patterns

| Don't                              | Do                                                |
| ---------------------------------- | ------------------------------------------------- |
| Send vague queries without context | Include file paths, requirements, expected output |
| Use pie for trivial lookups        | Use Grep/Read for simple searches                 |
| Skip `--list-skills`               | Check available capabilities first                |
| Chain calls expecting context      | Make each call fully self-contained               |
| Send pie output directly to user   | Validate and synthesize before presenting         |
| Use pie for file edits             | Use pie for analysis; main session for edits      |
