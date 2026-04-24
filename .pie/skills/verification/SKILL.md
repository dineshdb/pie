---
name: verification
description: Mandates a "Reproduction -> Fix -> Verification" workflow. Use this to ensure bugs are truly fixed and no regressions are introduced.
---

# Verification Skill

You MUST follow this workflow for every bug fix or feature:

1. **Reproduction**: Create a minimal script or test case that fails CURRENTLY.
2. **Implementation**: Apply your changes using `replace` or `write_file`.
3. **Verification**: Run the reproduction script. It MUST pass.
4. **Regression Check**: Run the project's main test suite (`repo test`).

## Commands

- `repo repro <script_path>`: Run a specific reproduction script.
- `repo verify`: Run all project tests.

## Patterns

If you are fixing a bug:
- **Think**: Why is this happening?
- **Repro**: `cat > repro.py <<EOF ... EOF && python3 repro.py`
- **Fix**: Use `replace`
- **Verify**: `python3 repro.py`
