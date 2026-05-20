---
name: debugging
description: Systematic root cause analysis — find the cause before fixing.
needs: [verification]
---

# Debugging

**Iron Law**: NO FIXES WITHOUT ROOT CAUSE INVESTIGATION.

## Phase 1: Root Cause

Before any fix:

1. **Read error completely** — stack traces, line numbers, error codes
2. **Reproduce consistently** — exact steps to trigger
3. **Check recent changes** — `git diff`, new deps, config changes
4. **Trace data flow** — where does the bad value originate?

Multi-component tracing:
```bash
# Trace a value through the stack
grep -rn "VARIABLE_NAME" src/
git log --oneline -10  # recent changes
git diff HEAD~3 -- src/module/  # what changed recently
```

Stack trace when lost:
```python
import traceback; traceback.print_stack()
# or in Rust: println!("{:?}", std::backtrace::Backtrace::capture());
```

## Phase 2: Pattern Analysis

1. Find working examples in codebase: `grep -rn "working_pattern" src/`
2. Read reference implementation completely
3. Identify ALL differences between working and broken
4. Understand dependencies and assumptions

## Phase 3: Hypothesis

1. State: "I think X is root cause because Y"
2. Make SMALLEST possible change
3. Test ONE variable at a time
4. **If 3+ fixes failed → question architecture**

## Phase 4: Implement + Defense-in-Depth

1. Create failing test case
2. Implement single fix (no "while I'm here" changes)
3. Verify fix works using `verification` skill
4. Add validation at every layer:

| Layer          | Purpose                                 |
| -------------- | --------------------------------------- |
| Entry          | Reject invalid input at boundary        |
| Business Logic | Ensure data makes sense for operation   |
| Environment    | Prevent dangerous operations in context |
| Debug          | Capture context for forensics           |

## Red Flags — STOP

- "Quick fix for now"
- "Just try changing X"
- "Add multiple changes at once"
- "I don't fully understand but this might work"
- **One more fix** (when 2+ already failed)

**All mean**: Return to Phase 1. If 3+ failed → question architecture.
