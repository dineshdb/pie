---
name: review
description: Comprehensive code review covering correctness, security, performance, architecture, and maintainability.
dependencies: [filesystem, developer]
---

# Review Skill Instructions

## Goal
Provide a structured, actionable code review. Identify real issues with root
cause analysis, not surface-level style nits. Every finding must include: what,
why it matters, where (file:line), and a concrete fix suggestion.

## Scope
- **target** (required): `"whole_repo"`, a `"branch_name"`, or a `"file_path"`.
- **focus** (optional): `"bugs"`, `"security"`, `"performance"`, `"architecture"`,
  `"all"`. Default is `"all"`.

---

## Review Workflow

### Step 1: Gather Context (Read-Only)

Before reviewing any code, understand what you're looking at.

```bash
# Project type and dependencies
cat -n Cargo.toml             # or package.json, pyproject.toml
cat -n src/lib.rs             # or __init__.py, index.ts

# Scope-specific exploration
git diff main...HEAD --stat   # For branch reviews
find . -name '*.rs' -not -path '*/target/*' | head -50  # File inventory

# Recent changes
git log --oneline -20
git diff HEAD~5..HEAD --stat
```

**Rules:**
- Read every file you plan to comment on
- Understand the module boundaries and data flow before criticizing structure
- Check git blame for context on *why* code exists, not just *what* it does

### Step 2: Systematic Review

Work through the focus areas below in order. For each finding, record:
- **Severity**: `critical` / `warning` / `info`
- **Category**: which focus area it belongs to
- **Location**: `file:line`
- **Issue**: what's wrong
- **Fix**: concrete suggestion (code snippet or approach)

### Step 3: Prioritize and Report

Present findings in severity order. Group related findings. Provide an executive
summary at the top.

---

## Focus Area 1: Bugs and Correctness

The highest priority. Wrong code is worse than ugly code.

### Error Handling

```bash
# Find bare unwrap/expect in non-test code
grep -rn '\.unwrap()' src/ | grep -v test | grep -v '#\[cfg(test)\]'
grep -rn '\.expect(' src/ | grep -v test

# Find ignored errors
grep -rn 'let _ = ' src/
grep -rn 'let Ok(_) = ' src/

# Find panicking paths
grep -rn 'panic!' src/ | grep -v test
grep -rn 'todo!' src/
grep -rn 'unimplemented!' src/
```

**Check for:**
- `.unwrap()` / `.expect()` on user input, file I/O, network calls, or parsed data
- Swallowed errors (`let _ = ...`, empty `catch` blocks)
- Missing error propagation in fallible operations
- Incorrect assumption about data shape (missing validation at boundaries)
- Race conditions in async code (shared mutable state without proper synchronization)
- Off-by-one errors, especially in indexing and range operations
- Integer overflow/underflow in arithmetic
- Missing `None` / `Err` branch in pattern matches

### Logic Errors

```bash
# Find early returns that might skip cleanup
grep -rn 'return ' src/ | head -40

# Find boolean conditions that look suspicious
grep -rnE 'if .* \|\| .* &&' src/   # Operator precedence issues
grep -rn 'if let Some.* = .*' src/  # Shadowed variables
```

**Check for:**
- Dead code paths (unreachable branches, contradictory conditions)
- Variable shadowing that hides bugs
- Assignment in conditionals (`if x = 5` instead of `if x == 5`)
- Reversed or incorrect comparison operators
- Missing `break` or `continue` in loops
- Resource leaks (unclosed files, connections, missing `defer`/`drop`)

---

## Focus Area 2: Security

Security issues can cause real harm. Treat these as critical.

### Input Validation

```bash
# Find string formatting in SQL/context where injection is possible
grep -rn 'format!.*SELECT\|format!.*INSERT\|format!.*UPDATE\|format!.*DELETE' src/
grep -rnE 'execute\(|query\(' src/ | grep -v 'prepared\|param\|bind'

# Find command injection vectors
grep -rnE 'Command::new|std::process::Command|shell_exec|exec\(' src/
grep -rn 'format!.*sh.*-c' src/

# Find deserialization of untrusted input
grep -rn 'serde_json::from_str\|from_reader\|json.loads' src/ | grep -v test

# Find path traversal risks
grep -rnE 'PathBuf::from.*\+|Path::new.*\+' src/
```

**Check for:**
- SQL injection (string interpolation in queries instead of parameterized)
- Command injection (user input in shell commands)
- Path traversal (unsanitized user input in file paths)
- Deserialization of untrusted data without validation
- Missing authentication/authorization checks
- Hardcoded secrets, API keys, tokens, passwords
- Insecure default configurations (TLS disabled, debug mode on)

### Secrets and Credentials

```bash
# Find potential secrets
grep -rnE '(password|secret|api_key|token|credential)\s*[:=]' src/ | grep -v test
grep -rnE '-----BEGIN (RSA |EC )?PRIVATE KEY-----' src/

# Find overly permissive file operations
grep -rn 'chmod.*777\|Permissions::from_mode.*0o777' src/
```

---

## Focus Area 3: Performance

Only flag performance issues that have measurable impact, not theoretical ones.

### Common Patterns to Check

```bash
# Find O(n^2) patterns: cloning inside loops
grep -rn '\.clone()' src/ | grep -v test | head -30

# Find unnecessary allocations
grep -rn '\.to_string()\|\.to_owned()' src/ | head -30
grep -rn 'String::from\|format!' src/ | grep -v 'error\|Error\|debug\|trace\|info\|warn' | head -20

# Find synchronous blocking in async contexts
grep -rn 'std::thread::sleep\|std::fs::\|std::net::' src/

# Find unbounded collections
grep -rnE 'Vec::new\(\)|Vec::with_capacity\(\)|HashMap::new\(\)' src/ | head -20
```

**Check for:**
- Unnecessary cloning / allocation (can a reference be used instead?)
- O(n^2) or worse algorithms where O(n) or O(n log n) exists
- Blocking I/O in async contexts
- Missing capacity hints on `Vec::with_capacity` / `HashMap::with_capacity` when size is known
- Repeated computation of the same value (missing memoization or caching)
- Large data structures held longer than needed (missing scoping)
- N+1 query patterns (one query per item in a loop instead of batch)
- Unbounded growth (collections that grow without limit)

### Memory

```bash
# Find potential memory hogs
grep -rn 'collect::<Vec\|collect::<HashMap\|collect::<String' src/ | head -20

# Find Arc/Mutex that might be unnecessary
grep -rn 'Arc<Mutex\|Arc<RwLock' src/
```

---

## Focus Area 4: Architecture and Design

Structural issues that make the codebase harder to evolve.

### Module Structure

```bash
# Understand the module hierarchy
find src -name 'mod.rs' -o -name 'lib.rs' | sort
cat -n src/lib.rs

# Check for circular dependencies
grep -rn 'use crate::' src/ | head -40
grep -rn 'use super::' src/ | head -20

# Find god modules (too many exports)
grep -rn 'pub fn\|pub struct\|pub enum\|pub trait' src/ | cut -d: -f1 | sort | uniq -c | sort -rn | head -10
```

**Check for:**
- Circular dependencies between modules
- God modules / god functions (doing too many things)
- Tight coupling between components that should be independent
- Missing abstractions where multiple implementations exist (no trait/interface)
- Leaky abstractions (internal details exposed through public API)
- Inconsistent patterns (same problem solved differently in different places)
- Dead code / unused modules
- Pub items that should be private (over-exposed API surface)

### Dependency Management

```bash
# Check dependency tree for bloat
cargo tree --depth 1 2>/dev/null || true

# Check for duplicate functionality in dependencies
cat -n Cargo.toml | grep -A1 '\[dependencies\]'
```

**Check for:**
- Dependencies that could be replaced with stdlib or a few lines of code
- Multiple dependencies solving the same problem
- Outdated or unmaintained dependencies
- Feature flags that pull in unnecessary transitive deps

---

## Focus Area 5: Maintainability and Readability

Code is read far more often than written. Make it easy to understand.

### Naming and Clarity

```bash
# Find single-letter variables (beyond loop counters)
grep -rnE 'let [a-z] ' src/ | grep -v 'let i\|let j\|let k\|let n\|let e\|let x\|let y' | head -20

# Find magic numbers
grep -rnE '[^a-zA-Z_][0-9]{2,}[^a-zA-Z_0-9.]' src/ | grep -v '0x\|test\|const\|static' | head -20

# Find deeply nested code
grep -rn '                    ' src/ | head -10  # 5+ levels of indentation
```

**Check for:**
- Magic numbers without named constants
- Single-letter or abbreviated variable names that lack context
- Functions longer than ~50 lines (usually doing too much)
- Deeply nested control flow (3+ levels of nesting → extract a function)
- Commented-out code (delete it, git remembers)
- Comments that restate what the code does instead of explaining *why*
- Inconsistent naming conventions (mixing styles within a module)

### Test Coverage

```bash
# Find modules without tests
grep -rL '#\[cfg(test)\]\|mod tests\|#\[test\]' src/ | head -20

# Count test-to-code ratio
grep -rc '#\[test\]' src/ | grep -v ':0$'
grep -rc 'pub fn\|pub async fn' src/ | grep -v ':0$'
```

**Check for:**
- Missing tests for critical paths (error handling, edge cases, boundary conditions)
- Tests that only cover the happy path
- Flaky tests (timing-dependent, order-dependent, platform-dependent)
- Test code that's harder to understand than the code it tests
- Missing test isolation (tests that share mutable state)

---

## Review Output Format

Present findings in this structure:

```
## Summary
<1-2 sentence overview of code quality and most critical finding>

## Critical Issues
1. [BUG] file:line — description
   Root cause: ...
   Fix: ...

## Warnings
1. [SECURITY] file:line — description
   Risk: ...
   Fix: ...

## Info / Suggestions
1. [PERFORMANCE] file:line — description
   Impact: ...
   Suggestion: ...

## Positive Patterns
- <call out good practices observed>
```

**Rules:**
- Every finding has a location (file:line), not vague "somewhere in X"
- Every finding has a root cause, not just a symptom description
- Every finding has a concrete fix suggestion
- Never suggest style-only changes without also identifying real issues
- Always acknowledge good patterns, not just problems
- Prioritize correctness > security > performance > maintainability > style
