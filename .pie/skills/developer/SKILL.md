---
name: developer
description: Development workflow - understand, plan, edit, verify, fix cycle with language-specific commands
---

## Development Loop

For any development task, follow this iterative cycle:

```
Understand -> Plan -> Edit -> Verify -> Fix -> (repeat if needed)
```

---

## Step 1: Understand (Read-Only Phase)

Before writing ANY code, gather complete context.

```bash
# Read the files you'll modify
cat -n <file>

# Find all references to relevant symbols
grep -rn '<symbol>' .

# Understand the project structure
find . -type f -name '*.rs' | head -50

# Check the build configuration
cat -n Cargo.toml

# Check recent changes
git log --oneline -10
git diff HEAD
```

**Rules:**

- Read every file you plan to modify
- Understand the data flow through the code
- Identify existing patterns and follow them
- Check for existing tests related to your changes
- Look at imports, dependencies, and module structure

---

## Step 2: Plan

Before coding, identify:

- Which files need changes
- What the expected behavior is
- How you will verify correctness
- Dependencies between changes (what must change first)
- Whether new files are truly needed (prefer editing existing files)

---

## Step 3: Edit

Make minimal, targeted changes. Use the filesystem skill patterns for file
manipulation.

```bash
# Use Python for reliable edits (recommended)
python3 << 'PYEOF'
path = "src/main.rs"
with open(path) as f:
    content = f.read()
content = content.replace("old code", "new code")
with open(path, "w") as f:
    f.write(content)
print("Edited", path)
PYEOF

# Or create new files when necessary
mkdir -p src/new_module
cat > src/new_module/mod.rs << 'EOF'
// content
EOF
```

**Rules:**

- One logical change at a time
- Match existing code style exactly (indentation, naming, patterns)
- No speculative changes or "while I'm here" improvements
- No boilerplate, dead code, or premature abstractions
- Prefer composition over inheritance
- Early returns over deep nesting

---

## Step 4: Verify

After EVERY change, run verification. Do not proceed until the current change is
confirmed working.

```bash
# Rust
cargo check                    # Quick type check (fastest feedback)
cargo build                    # Full build
cargo test                     # All tests
cargo test <test_name>         # Specific test
cargo clippy                   # Lint

# Python
uv run pytest -xvs             # Tests, stop on first failure, verbose
uv run pytest <file>           # Test specific file
uv run ruff check              # Lint
uv run ruff check --fix        # Lint and auto-fix
uv run pyright                 # Type check

# Shell
shellcheck <script>            # Lint shell scripts
bash -n <script>               # Syntax check only

# General
<build_command>                # Whatever the project uses
<test_command>                 # Whatever the project uses
```

**Verification checklist:**

- Build compiles with zero errors
- Existing tests still pass
- New functionality works as expected
- No new warnings introduced

---

## Step 5: Fix

If verification fails:

1. **Read the complete error message** -- do not skim, read every line
2. **Identify the root cause** -- where did the bad value or state originate?
3. **Fix the cause, not the symptom** -- patches create technical debt
4. **Re-run verification** -- confirm the fix actually resolves the issue
5. **If 3+ fixes fail** -- STOP. Step back and reconsider the approach. The
   architecture or assumption may be wrong.

**Common root causes:**

- Missing import or wrong module path
- Type mismatch (check function signatures)
- Ownership/borrowing issue (Rust)
- Incorrect variable name or scope
- Wrong assumption about data format or API response

---

## Debugging

### Reproduce First

```bash
# Run the failing command and capture all output
<command> 2>&1

# Run with verbose output
RUST_BACKTRACE=1 cargo test <test_name> 2>&1
RUST_LOG=debug cargo run 2>&1
RUST_BACKTRACE=full cargo run 2>&1

# Run a single test in isolation
cargo test <test_name> -- --nocapture
```

### Trace the Issue

```bash
# Check recent changes that might have caused it
git diff HEAD~1

# Find where a value or function is defined
grep -rn 'fn <name>' src/

# Find all usages of a symbol
grep -rn '<symbol>' src/

# Read the relevant function in context
cat -n <file>
```

### Debug Patterns

| Symptom           | First Check                                                            |
| ----------------- | ---------------------------------------------------------------------- |
| Build error       | Read the error at the specific line number, check types and signatures |
| Test failure      | Run just that test, read the assertion and actual vs expected          |
| Runtime panic     | Look at the stack trace, find the `.unwrap()` or `.expect()`           |
| Wrong output      | Add debug prints, trace the data flow from input to output             |
| Permission denied | Check file permissions: `ls -la <file>`                                |
| File not found    | Check working directory: `pwd`, verify path: `test -f <path>`          |

### Add Temporary Debug Output

```bash
# In Rust
println!("DEBUG: value = {:?}", value);
eprintln!("DEBUG: entered function");

# In Python
print(f"DEBUG: value = {value}", file=sys.stderr)
import pdb; pdb.set_trace()  # Interactive debugger
```

---

## Language-Specific Patterns

### Rust

```bash
cargo init <name>              # New binary project
cargo init --lib <name>        # New library project
cargo add <crate>              # Add dependency
cargo check                    # Fast type-check (no codegen)
cargo build                    # Full build
cargo build --release          # Optimized build
cargo test                     # All tests
cargo test <name>              # Test matching name
cargo test -- <name>           # Run by exact name filter
cargo test -- --nocapture      # Show print output
cargo clippy                   # Lint with Clippy
cargo clippy -- -W clippy::all # Strict linting
cargo fmt                      # Auto-format
cargo fmt -- --check           # Check formatting without changing
cargo doc --open               # Generate and open docs
cargo tree                     # Show dependency tree
```

**Rust patterns:**

- Use `anyhow` for applications, `thiserror` for libraries
- Use `Result<T>` with `?` operator instead of `.unwrap()` in production code
- Prefer iterators over explicit loops
- Use `impl Trait` for return types when applicable
- Derive `Debug` on all structs

### Python

```bash
uv init <name>                 # New project
uv add <package>               # Add dependency
uv run python <file>           # Run script
uv run pytest -xvs             # Tests, stop on first failure, verbose
uv run pytest <path>           # Test specific file or directory
uv run pytest -k <pattern>     # Test matching pattern
uv run pytest --lf             # Re-run last failed tests
uv run ruff check              # Lint
uv run ruff check --fix        # Auto-fix lint issues
uv run ruff format             # Format
uv run pyright                 # Type check
```

**Python patterns:**

- Use `pydantic` for data validation
- Use `pathlib.Path` instead of `os.path`
- Use f-strings for formatting
- Use type hints on all public functions
- Use `uv` for package management

### Shell Scripting

```bash
# Always start scripts with:
set -euo pipefail

# Check required tools exist
command -v rg >/dev/null || { echo "ripgrep required"; exit 1; }

# Safe temp files
tmpdir=$(mktemp -d)
trap 'rm -rf "$tmpdir"' EXIT
```

---

## Project Exploration

When approaching an unfamiliar codebase:

```bash
# Project structure
find . -type f -not -path '*/target/*' -not -path '*/.git/*' -not -path '*/node_modules/*' | sort | head -100

# Build configuration
cat -n Cargo.toml         # or package.json, pyproject.toml, etc.

# Entry point
cat -n src/main.rs         # or main.py, index.ts, etc.

# Module structure
cat -n src/lib.rs           # or mod.rs, __init__.py

# Test locations
find . -name '*test*' -type f -not -path '*/target/*' | head -20

# Recent activity
git log --oneline -20
git log --oneline --all --graph -20

# Key patterns
grep -rn 'struct\|enum\|trait\|impl' src/ | head -40     # Rust
grep -rn 'class\|def\|async def' src/ | head -40          # Python
grep -rn 'export\|function\|const' src/ | head -40        # TypeScript
```

---

## Principles

- **Read before write** -- Always understand existing code first
- **Small verified steps** -- Make one change, verify, then proceed to the next
- **Root cause over symptoms** -- Fix what is actually broken, not what looks
  broken
- **No speculative code** -- Only write what is needed for the current task
- **Follow existing patterns** -- Match the codebase's established conventions
- **Test your changes** -- If tests exist, run them; if none exist, consider
  adding them
- **Minimize changes** -- The best code is no code; every line is a liability
- **Correctness first** -- Make it work, make it clean, make it fast (in that
  order)
