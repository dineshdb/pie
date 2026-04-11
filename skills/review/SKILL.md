---
name: review
description: Comprehensive code review covering correctness, security, performance, architecture, and maintainability.
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

## Execution Rules

- You MUST execute the diagnostic commands in each focus area you are reviewing.
  These are tool calls, not optional examples.
- If a grep returns empty results, note that as a positive finding (e.g., "No
  bare unwrap() found in non-test code — good").
- Complete the review across all requested focus areas before producing the final
  report.
- If you find yourself wanting to ask a question, gather more context first —
  read the file, check git blame, run a grep. Most questions answer themselves
  with more exploration.
- Adapt the diagnostic commands to the project's language. The commands below
  use generic patterns; adjust file extensions and tool names as needed
  (e.g., `.py` instead of `.rs`, `pytest` instead of `cargo test`).

---

## Review Workflow

### Step 1: Explore the Project

Use /explore to understand the project structure, language, and dependencies.
Read every file you plan to comment on. For branch reviews, scope with
`git diff`:

```bash
git diff main...HEAD --stat
git diff main...HEAD
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

### Execute these diagnostic commands (adapt to project language):

```bash
# Find error suppression patterns
# Rust: .unwrap(), .expect()
# Python: bare except, pass in except blocks
# Go: ignoring error return values
# TypeScript: any casts, non-null assertions
grep -rn '\.unwrap()\|\.expect(' src/ | grep -v test | grep -v '#\[cfg(test)\]'
grep -rn 'let _ = ' src/
grep -rn 'panic!\|todo!\|unimplemented!' src/ | grep -v test

# Python-specific (run if Python project)
grep -rn 'except:\|except Exception:\|pass' src/ | head -30
grep -rn '# type: ignore' src/ | head -20

# Find early returns that might skip cleanup
grep -rn 'return ' src/ | head -40

# Find suspicious boolean conditions
grep -rnE 'if .* \|\| .* &&' src/
```

**Check results for:**
- Error suppression (`.unwrap()`, bare `except`, `let _ = ...`, ignored return values)
- Missing error propagation in fallible operations
- Incorrect assumption about data shape (missing validation at boundaries)
- Race conditions in async/concurrent code (shared mutable state without synchronization)
- Off-by-one errors in indexing and range operations
- Integer overflow/underflow
- Missing `None` / `null` / `Err` / `undefined` handling
- Dead code paths (unreachable branches, contradictory conditions)
- Variable shadowing that hides bugs
- Resource leaks (unclosed files, connections, missing cleanup)

---

## Focus Area 2: Security

Security issues can cause real harm. Treat these as critical.

### Execute these diagnostic commands:

```bash
# Find injection vectors in queries
grep -rnE 'format!.*SELECT\|format!.*INSERT\|f".*SELECT\|f".*INSERT' src/
grep -rnE 'execute\(|query\(' src/ | grep -v 'prepared\|param\|bind'

# Find command injection vectors
grep -rnE 'Command::new|subprocess\.|os\.system\|exec\(|shell=True' src/
grep -rn 'format!.*sh.*-c' src/

# Find deserialization of untrusted input
grep -rn 'serde_json::from_str\|json.loads\|yaml.load\|pickle.loads\|eval(' src/ | grep -v test

# Find path traversal risks
grep -rnE 'PathBuf::from.*\+|os\.path\.join.*\+|path\.join.*input' src/

# Find potential secrets
grep -rnE '(password|secret|api_key|token|credential)\s*[:=]' src/ | grep -v test
grep -rnE '-----BEGIN (RSA |EC )?PRIVATE KEY-----' src/

# Find overly permissive file operations
grep -rn 'chmod.*777\|Permissions::from_mode.*0o777\|0o777' src/
```

**Check results for:**
- SQL injection (string interpolation in queries instead of parameterized)
- Command injection (user input in shell commands)
- Path traversal (unsanitized user input in file paths)
- Deserialization of untrusted data without validation
- Missing authentication/authorization checks
- Hardcoded secrets, API keys, tokens, passwords
- Insecure default configurations (TLS disabled, debug mode on, CORS wide open)

---

## Focus Area 3: Performance

Only flag performance issues that have measurable impact, not theoretical ones.

### Execute these diagnostic commands:

```bash
# Find unnecessary copies/allocations
grep -rn '\.clone()\|\.copy()' src/ | grep -v test | head -30
grep -rn '\.to_string()\|\.to_owned()' src/ | head -30

# Find synchronous blocking in async contexts
grep -rn 'std::thread::sleep\|time\.sleep\|std::fs::\|os\.read' src/

# Find unbounded collections
grep -rnE 'Vec::new\(\)|HashMap::new\(\)|list()\|dict()' src/ | head -20

# Find potential memory hogs (large collects)
grep -rn 'collect\(' src/ | head -20

# Find heavy synchronization primitives
grep -rn 'Arc<Mutex\|Arc<RwLock\|synchronized\|Lock\|RLock' src/
```

**Check results for:**
- Unnecessary copying / allocation (can a reference be used instead?)
- O(n^2) or worse algorithms where O(n) or O(n log n) exists
- Blocking I/O in async contexts
- Missing capacity hints on collections when size is known
- Repeated computation of the same value (missing memoization/caching)
- Large data structures held longer than needed (missing scoping)
- N+1 query patterns (one query per item in a loop instead of batch)
- Unbounded growth (collections that grow without limit)

---

## Focus Area 4: Architecture and Design

Structural issues that make the codebase harder to evolve.

### Execute these diagnostic commands:

```bash
# Understand the module hierarchy
find src -name 'mod.rs' -o -name '__init__.py' -o -name 'index.ts' -o -name 'lib.rs' | sort

# Check import dependencies
grep -rn 'use crate::\|use super::\|from \.\|import \.\.' src/ | head -40

# Find god modules (too many exports)
grep -rn 'pub fn\|pub struct\|pub enum\|pub trait\|export\|export default' src/ | cut -d: -f1 | sort | uniq -c | sort -rn | head -10

# Find dead exports (public but unused)
grep -rn 'pub fn\|export function\|def ' src/ | head -30

# Check dependency tree
cat Cargo.toml 2>/dev/null || cat package.json 2>/dev/null || cat pyproject.toml 2>/dev/null || cat go.mod 2>/dev/null
```

**Check results for:**
- Circular dependencies between modules
- God modules / god functions (doing too many things)
- Tight coupling between components that should be independent
- Missing abstractions where multiple implementations exist (no trait/interface)
- Leaky abstractions (internal details exposed through public API)
- Inconsistent patterns (same problem solved differently in different places)
- Dead code / unused modules
- Over-exposed API surface (pub items that should be private)
- Redundant dependencies (could be replaced with stdlib or a few lines of code)
- Multiple dependencies solving the same problem

---

## Focus Area 5: Maintainability and Readability

Code is read far more often than written. Make it easy to understand.

### Execute these diagnostic commands:

```bash
# Find magic numbers
grep -rnE '[^a-zA-Z_][0-9]{2,}[^a-zA-Z_0-9.]' src/ | grep -v '0x\|test\|const\|static\|final' | head -20

# Find deeply nested code (5+ levels)
grep -rn '                    ' src/ | head -10

# Find modules without tests
grep -rL '#\[cfg(test)\]\|mod tests\|#\[test\]\|describe\|it(\|test_' src/ | head -20

# Count test-to-code ratio
grep -rc '#\[test\]\|def test_\|it(\|test(' src/ | grep -v ':0$'
grep -rc 'pub fn\|export function\|def ' src/ | grep -v ':0$'

# Find TODO/FIXME/HACK comments
grep -rn 'TODO\|FIXME\|HACK\|XXX' src/ | head -20
```

**Check results for:**
- Magic numbers without named constants
- Functions longer than ~50 lines (usually doing too much)
- Deeply nested control flow (3+ levels → extract a function)
- Commented-out code (delete it, git remembers)
- Comments that restate what the code does instead of explaining *why*
- Inconsistent naming conventions within a module
- Missing tests for critical paths (error handling, edge cases, boundaries)
- Tests that only cover the happy path
- Flaky tests (timing-dependent, order-dependent, platform-dependent)

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
- The review is COMPLETE when you have output findings for ALL requested focus areas
