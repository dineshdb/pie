---
name: verification
description: Reproduce bugs, validate fixes, and run regressions.
---

# Verification Workflow

Evidence-based development. No completion claims without fresh proof of success.

## 1. Reproduction (The "Failing State")
Before fixing, prove the bug exists with a minimal test or script.
```bash
cat > repro.py << 'EOF'
# Minimal code to trigger the failure
EOF
python3 repro.py # Should FAIL
```

## 2. Validation (The "Passing State")
After implementation, run the *same* reproduction script to confirm the fix.
```bash
python3 repro.py # Should now PASS
```

## 3. Regression Check
Ensure no collateral damage by running existing test suites.
```bash
cargo test
uv run pytest
repo verify
```

## Principles
- **Repro First**: Never fix a bug you haven't reproduced.
- **Isolate**: Keep reproduction scripts minimal and independent.
- **Clean Up**: Remove temporary repro scripts after verification is complete.
- **Evidence**: Prefer actual command output over "hand-wavy" assertions of success.
