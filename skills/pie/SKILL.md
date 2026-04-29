---
name: pie
description: Invoke the pie agent CLI — a non-interactive, one-shot AI assistant for research, code review, codebase exploration, docs lookup, planning, and code simplification. Use to offload work from the main Claude session and reduce token consumption.
---

# Pie CLI Agent

Pie is a non-interactive, one-shot analysis tool. It has no conversation history; every query must be fully self-contained.

## When to Invoke
- **Research**: Web/Doc search or framework analysis.
- **Review**: Correctness, security, or performance audits.
- **Planning**: Generating detailed implementation steps from context.
- **Simplification**: Finding dead code or duplication.

## Usage Patterns

### Mandatory Formats
Always use `--md` for markdown output.
```bash
pie --md "Your detailed, self-contained query"
```

### Self-Contained Queries
Include all relevant paths and requirements in a single call.
```bash
pie --md "Review src/main.rs for potential deadlocks in the session handling logic. 
Requirements: Ensure Arc/Mutex usage follows safety patterns. 
Output: ## Findings then ## Recommendations"
```

## Available Sub-Agents
- **docs**: Library API and usage patterns.
- **explore**: Project structure and architectural mapping.
- **plan**: Step-by-step implementation blueprints.
- **review**: Code quality and correctness audits.
- **simplify**: Codebase slimming and refactoring discovery.

## Workflow
1. `pie --list-skills` (Optional check)
2. `pie --md "query"`
3. Incorporate results into main session.
