---
name: code-review
description: Structured code review with SOLID, security, and quality checks.
needs: [verification]
---

# Code Review

Review git changes with severity levels. Default to review-only unless asked to fix.

## Preflight

```bash
git status -sb
git diff --stat
git diff
```

Edge cases:
- **No changes**: Ask if reviewing staged or a commit range.
- **Large diff (>500 lines)**: Summarize by file, review in batches by module.
- **Mixed concerns**: Group findings by feature, not file order.

## Severity Levels

| Level | Name     | Action                    |
| ----- | -------- | ------------------------- |
| **P0** | Critical | Must block merge          |
| **P1** | High     | Should fix before merge   |
| **P2** | Medium   | Fix in this PR or follow-up |
| **P3** | Low      | Optional improvement      |

## SOLID + Architecture Smells

| Principle | Smell                                      | Fix                         |
| --------- | ------------------------------------------ | --------------------------- |
| **SRP**   | Module with unrelated responsibilities     | Split by concern            |
| **OCP**   | Editing to add behavior instead of extending | Add extension point       |
| **LSP**   | Subclass breaks expectations, needs type checks | Fix abstraction        |
| **ISP**   | Wide interface with unused methods         | Narrow the interface        |
| **DIP**   | High-level logic tied to low-level impl    | Invert dependency           |

## Security Checklist

- SQL injection, XSS, command injection, SSRF, path traversal
- Auth bypass, missing tenancy checks
- Hardcoded secrets, API keys in logs
- Unsafe deserialization, weak crypto
- Race conditions: concurrent access, TOCTOU, missing locks

Quick scan:
```bash
rg -i "AKIA[0-9A-Z]{16}" .            # AWS keys
rg "ghp_[a-zA-Z0-9]{36}" .            # GitHub tokens
rg -i "password.*=" .                  # Hardcoded passwords
rg -i "eval\s*\(" .                    # Code injection
rg -i "SELECT.*\+.*FROM" .             # SQL concatenation
```

## Quality Checks

- **Error handling**: swallowed exceptions, broad catch, missing handling
- **Performance**: N+1 queries, CPU in hot paths, unbounded memory
- **Boundaries**: null handling, empty collections, numeric edges, off-by-one

## Removal Candidates

- Unused code, redundant implementations, feature-flagged-off code
- Classify: **safe delete now** vs **defer with plan**

## Output Format

```markdown
## Code Review Summary
**Files**: X files, Y lines changed
**Assessment**: APPROVE / REQUEST_CHANGES / COMMENT

### P0 - Critical
(none or list)

### P1 - High
1. **[file:line]** Title — description, suggested fix

### P2 - Medium
2. ...

### P3 - Low
3. ...
```

## Receiving Feedback Rules

- No "Great point!", "Thanks!", "You're right!"
- Unclear feedback → STOP and ask clarification
- Verify technically, push back if wrong
- YAGNI-check before implementing suggestions
