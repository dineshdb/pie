---
name: explore
description: Explore and understand a codebase — project type, structure, dependencies, and key files.
---

# Explore Skill

Gather comprehensive context about a project before modifying or reviewing it.
Adapts to the project's language and build system automatically.

## How to use

1. Run `repo context` first (Step 2) to get a project overview
2. Review the output — if it answers the user's question, respond immediately
3. Only proceed to deeper exploration (Steps 3-6) if more context is needed
4. Do NOT explore further when the overview suffices

## repo CLI

Quick project operations via `repo` CLI:

```bash
repo context    # Gather full project context in one call
repo build      # Build all detected projects
repo test       # Run all tests
repo lint       # Run all linters
repo fmt        # Format code
```

Run `repo --help` for all options.

## Step 1: Detect Environment

```bash
# Check if inside a git repo
git rev-parse --is-inside-work-tree 2>/dev/null && echo "IN_REPO" || echo "NOT_REPO"
```

## Step 2: Get Project Overview

If inside a git repo, use `repo context` for a single-call overview:

```bash
repo context
```

If not in a git repo, gather manually:

```bash
# Project type detection
ls Cargo.toml package.json pyproject.toml go.mod build.gradle pom.xml 2>/dev/null
ls -la
```

## Step 3: Identify Project Type

Based on what exists, identify the stack:

| File found          | Language  | Build       | Test              |
| ------------------- | --------- | ----------- | ----------------- |
| `Cargo.toml`        | Rust      | `cargo`     | `cargo test`      |
| `package.json`      | JS/TS     | `npm/pnpm`  | `npm test`        |
| `pyproject.toml`    | Python    | `uv/pip`    | `pytest`          |
| `go.mod`            | Go        | `go build`  | `go test`         |
| `build.gradle`      | Java/Kotlin | `gradle`  | `gradle test`     |
| `pom.xml`           | Java      | `mvn`       | `mvn test`        |

## Step 4: Read Key Files

Read files in this order, adapting to the detected project type:

```bash
# 1. Build config
cat -n Cargo.toml    # or package.json, pyproject.toml, go.mod

# 2. Entry point
cat -n src/main.rs   # or main.py, index.ts, main.go

# 3. Module structure (if exists)
cat -n src/lib.rs    # or __init__.py, mod.rs, index.ts
```

**Rule:** Only read `lib.rs`/`__init__.py` if it exists. Binary-only projects
don't have one. Check existence first:

```bash
test -f src/lib.rs && cat -n src/lib.rs || echo "No lib.rs (binary crate)"
```

## Step 5: Map Module Structure

```bash
# File inventory
find src -type f -not -path '*/target/*' -not -path '*/node_modules/*' | sort | head -50

# Public API surface
grep -rn 'pub fn\|pub struct\|pub enum\|pub trait\|export\|export default' src/ | head -30
```

## Step 6: Recent Activity (if git repo)

```bash
git log --oneline -10
git diff HEAD~5..HEAD --stat
git status --short
```

## Output

After exploration, you should understand:
- What language/framework the project uses
- How to build, test, and lint
- The module structure and data flow
- Recent changes and current work state

Use this context before making changes, writing reviews, or answering
architecture questions. Reading actual files prevents guessing.
