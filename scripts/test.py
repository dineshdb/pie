#!/usr/bin/env uv run
# /// script
# requires-python = ">=3.9"
# dependencies = ["pyyaml"]
# ///
"""YAML-driven integration tests for pie.

Usage: uv run tests/run.py           (run all)
       uv run tests/run.py offline    (skip tests needing a model)
"""

import re
import subprocess
import sys
import urllib.error
import urllib.request
from pathlib import Path

import yaml

ROOT = Path(__file__).resolve().parent.parent
PIE = ["cargo", "run", "--quiet", "--"]


def green(s):
    print(f"\033[32m{s}\033[0m")


def red(s):
    print(f"\033[31m{s}\033[0m")


def yellow(s):
    print(f"\033[33m{s}\033[0m")


def ensure_list(val):
    if val is None:
        return []
    return val if isinstance(val, list) else [val]


def run_pie(args, input_text=None, debug=False, timeout=30):
    cmd = PIE + args
    if input_text:
        cmd += [input_text]
        if debug:
            cmd.append("--debug")
    try:
        r = subprocess.run(
            cmd, capture_output=True, text=True, timeout=timeout, cwd=ROOT
        )
        return r.stdout + r.stderr, r.returncode
    except subprocess.TimeoutExpired:
        return "(timeout)", -1


def check_online():
    try:
        urllib.request.urlopen("http://127.0.0.1:8000/v1/models", timeout=2)
        return True
    except urllib.error.HTTPError:
        return True
    except Exception:
        return False


def run_test(test, online):
    name = test["name"]
    failures = []

    if test.get("skip") == "online" and not online:
        yellow(f"  SKIP: {name}")
        return "skip"

    args = test.get("args", "").split() if test.get("args") else []
    out, exit_code = run_pie(
        args,
        input_text=test.get("input"),
        debug=test.get("debug", False),
        timeout=test.get("timeout", 30),
    )

    check = out
    if test.get("filter"):
        check = "\n".join(re.findall(test["filter"], out))

    if "exit" in test and exit_code != test["exit"]:
        failures.append(f"exit: expected {test['exit']}, got {exit_code}")

    for pat in ensure_list(test.get("contains")):
        if pat not in check:
            failures.append(f"missing: {pat!r}")

    for pat in ensure_list(test.get("not_contains")):
        if pat in check:
            failures.append(f"unexpected: {pat!r}")

    if failures:
        red(f"  FAIL: {name}")
        for f in failures:
            red(f"    - {f}")
        for line in out.splitlines()[:5]:
            print(f"    {line}")
        return "fail"

    green(f"  PASS: {name}")
    return "pass"


def main():
    offline = len(sys.argv) > 1 and sys.argv[1] == "offline"

    online = not offline and check_online()
    if not offline and not online:
        yellow("WARNING: model server not reachable, skipping online tests")

    print("Building pie...")
    subprocess.run(["cargo", "build", "--quiet"], cwd=ROOT, check=True)
    print()

    with open("tests/tests.yaml") as f:
        tests = yaml.safe_load(f)["tests"]

    print("══ Running tests ══\n")

    passed = failed = skipped = 0
    for test in tests:
        result = run_test(test, online)
        if result == "pass":
            passed += 1
        elif result == "fail":
            failed += 1
        else:
            skipped += 1

    print(f"\n══ Results ══")
    green(f"  Passed: {passed}")
    print(f"  Failed: {failed}" if not failed else f"\033[31m  Failed: {failed}\033[0m")
    if skipped:
        yellow(f"  Skipped: {skipped}")
    else:
        print(f"  Skipped: {skipped}")

    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
