---
name: developer
description: Orchestrate research, planning, implementation, and verification.
needs: [git, filesystem, context7, verification]
---

# Development Lifecycle Orchestration

Follow this rigorous loop for any non-trivial task.

## Phase 1: Research & Discovery
Gather context before proposing changes.
- **Map Dependencies**: Use `filesystem` to find relevant files and imports.
- **Trace Logic**: Use `grep` to understand data flow and symbol usage.
- **Reference Docs**: Use `context7` for external libraries.
- **Check History**: Use `git` to see how code evolved.

## Phase 2: Strategy & Planning
Define the "What" and "How" before implementation.
- Identify every file to be modified.
- Outline the logic changes.
- **Define Verification**: Plan how success will be proven.

## Phase 3: Implementation (The Edit Loop)
Execute minimal, verified changes.
- Use `filesystem` for surgical edits.
- Follow existing project patterns and style.
- Prefer composition and simplicity over abstractions.

## Phase 4: Verification & Hardening
Apply the `verification` workflow.
- **Repro First**: Confirm the bug/absence of feature.
- **Verify Fix**: Prove the change works.
- **Regression Test**: Ensure no collateral damage.

## Core Principles
1. **Read before Write**: Context is everything.
2. **Surgical Precision**: Minimal changes for maximum effect.
3. **Evidence over Assertion**: Prove it works with output.
4. **Root Cause Fixes**: Fix the source, not the symptom.
5. **Simplify Continuously**: Leave the codebase cleaner than you found it.
