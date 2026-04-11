#!/usr/bin/env uv run
# /// script
# requires-python = ">=3.9"
# dependencies = ["pyyaml"]
# ///
"""YAML-driven integration tests for pie.

Usage: uv run scripts/test.py           (run all)
       uv run scripts/test.py offline    (skip tests needing a model)
"""

import re
import subprocess
import sys
import urllib.error
import urllib.request
from concurrent.futures import ThreadPoolExecutor, as_completed
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
        return name, "skip", []

    max_retries = 3 if test.get("skip") == "online" else 1

    for attempt in range(max_retries):
        failures = []

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

        # Run post-test shell command (exit code determines pass/fail)
        post_cmd = test.get("post")
        if post_cmd:
            try:
                r = subprocess.run(
                    post_cmd, shell=True, capture_output=True, text=True, timeout=10, cwd=ROOT
                )
                if r.returncode != 0:
                    preview_lines = (r.stdout + r.stderr).strip().splitlines()[:3]
                    failures.append(f"post: {post_cmd!r} exited {r.returncode}")
                    for line in preview_lines:
                        failures.append(f"  {line}")
            except subprocess.TimeoutExpired:
                failures.append(f"post: {post_cmd!r} timed out")

        if not failures:
            break

    preview = out.splitlines()[:5]
    status = "fail" if failures else "pass"
    return name, status, failures, preview


def print_result(result):
    name = result[0]
    status = result[1]
    if status == "skip":
        yellow(f"  SKIP: {name}")
    elif status == "fail":
        red(f"  FAIL: {name}")
        for f in result[2]:
            red(f"    - {f}")
        for line in result[3]:
            print(f"    {line}")
    else:
        green(f"  PASS: {name}")


def main():
    offline = len(sys.argv) > 1 and sys.argv[1] == "offline"

    online = not offline and check_online()
    if not offline and not online:
        yellow("WARNING: model server not reachable, skipping online tests")

    print("Building pie...")
    subprocess.run(["cargo", "build", "--quiet"], cwd=ROOT, check=True)
    print()

    with open(ROOT / "tests" / "tests.yaml") as f:
        tests = yaml.safe_load(f)["tests"]

    print("══ Running tests ══\n")

    # Split into sequential (instant) and parallel (API calls) groups
    sequential = [t for t in tests if not t.get("parallel")]
    parallel = [t for t in tests if t.get("parallel")]

    passed = failed = skipped = 0

    # Run sequential tests first (CLI flags, skills listing — instant)
    for test in sequential:
        result = run_test(test, online)
        print_result(result)
        _, status, *_ = result
        if status == "pass":
            passed += 1
        elif status == "fail":
            failed += 1
        else:
            skipped += 1

    # Run parallel tests concurrently (API calls — slow)
    if parallel:
        with ThreadPoolExecutor(max_workers=1) as pool:
            futures = {pool.submit(run_test, t, online): t for t in parallel}
            for future in as_completed(futures):
                result = future.result()
                print_result(result)
                _, status, *_ = result
                if status == "pass":
                    passed += 1
                elif status == "fail":
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
