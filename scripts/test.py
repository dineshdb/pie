#!/usr/bin/env uv run
# /// script
# requires-python = ">=3.9"
# dependencies = ["pyyaml", "python-dotenv"]
# ///
"""YAML-driven integration tests for pie.

Runs all tests with debug tracing, parses structured JSON events from
stderr, and validates tool calls, tasks, and response content.

Usage: uv run scripts/test.py
"""

import json
import os
import re
import subprocess
import sys
import urllib.error
import urllib.request
from pathlib import Path

import yaml
from dotenv import load_dotenv

ROOT = Path(__file__).resolve().parent.parent
PIE = ["cargo", "run", "--quiet", "--"]

ANALYSIS_PROMPT = """Analyze these debug logs from pie test runs for efficiency issues.
Focus on:
1. REDUNDANT TOOL CALLS: Same or similar commands run multiple times
2. UNNECESSARY AGENT SPAWNS: Subagent used when direct tool call would suffice
3. FAILED RETRIES: Tool calls that failed and had to be retried
4. EXCESSIVE BACK-AND-FORTH: More LLM round-trips than needed
5. MISSED PRELOADING: Skills mentioned in the query that weren't pre-loaded

For each issue, report what happened, why it's inefficient, and a suggested fix.
Also report: total LLM calls, total tool calls, efficiency score (1-5).
Be specific. Reference actual commands from the log.
If you don't find any issue, just name it and add a checkmark. No need for description.
Try to keep the output succint."""


def green(s):
    print(f"\033[32m{s}\033[0m")


def red(s):
    print(f"\033[31m{s}\033[0m")


def yellow(s):
    print(f"\033[33m{s}\033[0m")


def cyan(s):
    print(f"\033[36m{s}\033[0m")


def ensure_list(val):
    if val is None:
        return []
    return val if isinstance(val, list) else [val]


def run_pie(args, input_text=None, timeout=30, provider=None):
    """Run pie and return (stdout, stderr, exit_code) separately."""
    cmd = PIE[:]
    if provider:
        cmd += ["-p", provider]
    cmd += args
    if input_text:
        cmd += [input_text]
    cmd.append("--debug")
    cmd.append("--md")
    try:
        r = subprocess.run(
            cmd, capture_output=True, text=True, timeout=timeout, cwd=ROOT
        )
        return r.stdout, r.stderr, r.returncode
    except subprocess.TimeoutExpired:
        return "(timeout)", "", -1


def _strip_ansi(s):
    """Remove ANSI escape codes from a string."""
    return re.sub(r"\x1b\[[0-9;]*m", "", s)


def parse_events(stderr):
    """Parse TOOL:/PROGRESS: lines from pie stderr.

    Returns (tool_names, task_titles, progress, tool_inputs).
    - tool_names: set of tool names that were called
    - task_titles: list of all task titles from task_add events
    - progress: list of progress summary strings
    - tool_inputs: dict mapping tool name -> list of parsed JSON data
    """
    tool_names = set()
    task_titles = []
    progress = []
    tool_inputs = {}

    for line in stderr.splitlines():
        raw = line.strip()

        # TOOL: <name> <optional_json>
        if raw.startswith("TOOL: "):
            rest = raw[6:]
            parts = rest.split(" ", 1)
            name = parts[0]
            tool_names.add(name)

            if len(parts) > 1:
                try:
                    data = json.loads(parts[1])
                except json.JSONDecodeError:
                    data = None
                tool_inputs.setdefault(name, []).append(data)

                if name == "task_add" and isinstance(data, dict):
                    # Input format: {"tasks": [{"title": "...", "status": "..."}]}
                    # Also accepts "name" as alias for "title"
                    for task in data.get("tasks", []):
                        if isinstance(task, dict):
                            title = task.get("title", "") or task.get("name", "")
                            if title:
                                task_titles.append(title)
                        elif isinstance(task, str):
                            task_titles.append(task)

        # PROGRESS: <summary>
        elif raw.startswith("PROGRESS: "):
            progress.append(raw[10:])

    return tool_names, task_titles, progress, tool_inputs


def _input_matches(expected, actual):
    """Check if expected is a subset of actual.

    - If both are dicts: every key in expected must exist in actual with a matching value
    - If expected is a dict but actual is a string (e.g. Rust Debug format):
      check each key/value appears in the string
    - If expected is a string: substring match against str(actual)
    - Otherwise: equality check
    """
    if isinstance(expected, dict) and isinstance(actual, dict):
        return all(
            _input_matches(expected[k], actual.get(k))
            for k in expected
        )
    if isinstance(expected, dict) and isinstance(actual, str):
        # Rust Debug format: Object {"key": String("value")}
        # Check each expected key and value appear in the string
        for key, val in expected.items():
            if f'"{key}"' not in actual:
                return False
            if isinstance(val, str) and val not in actual:
                return False
        return True
    if isinstance(expected, str):
        return expected in str(actual)
    return expected == actual


def validate_structured(stderr, test):
    """Validate structured assertions (tools, tasks, task_count, tool_calls) against stderr."""
    failures = []
    tool_names, task_titles, _progress, tool_inputs = parse_events(stderr)

    # Check required tool names
    for required in ensure_list(test.get("tools")):
        if required not in tool_names:
            failures.append(f"missing tool: {required!r} (found: {sorted(tool_names)})")

    # Check tool call input/output parameters
    for call in ensure_list(test.get("tool_calls")):
        name = call["name"]
        if name not in tool_inputs:
            failures.append(f"tool {name!r} was never called (found: {sorted(tool_inputs)})")
            continue
        if "input" in call:
            expected = call["input"]
            found = any(
                _input_matches(expected, actual)
                for actual in tool_inputs[name]
                if actual is not None
            )
            if not found:
                failures.append(
                    f"tool {name!r} input not matched: expected {expected!r}"
                )

    # Check required task title substrings
    for substr in ensure_list(test.get("tasks")):
        found = any(substr.lower() in t.lower() for t in task_titles)
        if not found:
            failures.append(
                f"missing task with {substr!r} (tasks: {task_titles})"
            )

    # Check minimum task count
    min_count = test.get("task_count")
    if min_count is not None and len(task_titles) < min_count:
        failures.append(
            f"expected >= {min_count} tasks, got {len(task_titles)}"
        )

    return failures


def validate_response(stdout, test):
    """Validate response content assertions against stdout."""
    failures = []

    for pat in ensure_list(test.get("contains")):
        if pat not in stdout:
            failures.append(f"missing in response: {pat!r}")

    for pat in ensure_list(test.get("not_contains")):
        if pat in stdout:
            failures.append(f"unexpected in response: {pat!r}")

    return failures


def extract_trace(stderr, stdout):
    """Extract concise trace from TOOL:/PROGRESS: lines for LLM analysis."""
    lines = []

    for line in stderr.splitlines():
        raw = line.strip()
        if raw.startswith("TOOL: ") or raw.startswith("PROGRESS: "):
            lines.append(f"  {raw}")

    return "\n".join(lines)


def run_test(test, provider=None):
    """Run a single test and return (name, status, failures, trace)."""
    name = test["name"]
    failures = []

    args = test.get("args", "").split() if test.get("args") else []
    stdout, stderr, exit_code = run_pie(
        args,
        input_text=test.get("input"),
        timeout=test.get("timeout", 30),
        provider=provider,
    )

    # Exit code check
    if "exit" in test and exit_code != test["exit"]:
        failures.append(f"exit: expected {test['exit']}, got {exit_code}")

    # Response content checks (stdout only)
    failures.extend(validate_response(stdout, test))

    # Structured event checks (stderr only)
    failures.extend(validate_structured(stderr, test))

    # Post-run command
    post_cmd = test.get("post")
    if post_cmd:
        try:
            r = subprocess.run(
                post_cmd,
                shell=True,
                capture_output=True,
                text=True,
                timeout=10,
                cwd=ROOT,
            )
            if r.returncode != 0:
                failures.append(f"post: {post_cmd!r} exited {r.returncode}")
        except subprocess.TimeoutExpired:
            failures.append(f"post: {post_cmd!r} timed out")

    trace = extract_trace(stderr, stdout)
    status = "fail" if failures else "pass"
    return name, status, failures, trace


def print_result(name, status, failures):
    if status == "fail":
        red(f"  FAIL: {name}")
        for f in failures:
            red(f"    - {f}")
    else:
        green(f"  PASS: {name}")


def analyze_traces(traces):
    """Single LLM call to analyze all test traces for efficiency."""
    combined = []
    for name, trace in traces:
        combined.append(f"=== TEST: {name} ===\n{trace}\n")
    full_log = "\n".join(combined)

    base_url = (
        os.environ.get("OPENAI_BASE_URL")
        or "http://127.0.0.1:8000/v1"
    ).rstrip("/")
    model = os.environ.get("OPENAI_MODEL", "")
    api_key = os.environ.get("OPENAI_API_KEY", "ollama")

    payload = json.dumps(
        {
            "model": model,
            "messages": [
                {"role": "system", "content": ANALYSIS_PROMPT},
                {"role": "user", "content": full_log},
            ],
            "temperature": 0.3,
        }
    ).encode()

    req = urllib.request.Request(
        f"{base_url}/chat/completions",
        data=payload,
        headers={
            "Content-Type": "application/json",
            "Authorization": f"Bearer {api_key}",
        },
    )
    try:
        with urllib.request.urlopen(req, timeout=120) as resp:
            data = json.loads(resp.read())
            return data["choices"][0]["message"]["content"].strip()
    except urllib.error.HTTPError as e:
        body = e.read().decode()[:500]
        return f"(analysis failed: HTTP {e.code} - {body})"
    except Exception as e:
        return f"(analysis failed: {e})"


def main():
    load_dotenv(ROOT / ".env")

    # Resolve provider: if OPENAI_BASE_URL is set, derive provider name from
    # pie.toml config that matches the base_url. Otherwise fall back to None
    # (pie uses its default_provider).
    provider = None
    env_url = os.environ.get("OPENAI_BASE_URL", "").rstrip("/")
    if env_url:
        # Find a pie.toml provider matching the env base_url
        config_paths = [
            Path.home() / ".pie" / "pie.toml",
            ROOT / ".pie" / "pie.toml",
        ]
        for config_path in config_paths:
            if not config_path.exists():
                continue
            try:
                import tomllib
                with open(config_path, "rb") as f:
                    pie_cfg = tomllib.load(f)
                for name, cfg in pie_cfg.get("provider", {}).items():
                    if cfg.get("base_url", "").rstrip("/") == env_url:
                        provider = name
                        break
            except (ImportError, Exception):
                pass
            if provider:
                break

    print("Building pie...")
    subprocess.run(["cargo", "build", "--quiet"], cwd=ROOT, check=True)
    if provider:
        print(f"Provider: {provider}")
    print()

    with open(ROOT / "tests" / "tests.yaml") as f:
        tests = yaml.safe_load(f)["tests"]

    traces = []
    passed = failed = 0

    for test in tests:
        name, status, failures, trace = run_test(test, provider=provider)
        print_result(name, status, failures)
        if status == "pass":
            passed += 1
        else:
            failed += 1
        if trace:
            traces.append((name, trace))

    print(f"\n== Results ==")
    green(f"  Passed: {passed}")
    if failed:
        red(f"  Failed: {failed}")
    else:
        print(f"  Failed: {failed}")

    if traces:
        print("\n== Efficiency Analysis ==\n")
        analysis = analyze_traces(traces)
        if analysis:
            cyan(analysis)
        else:
            yellow("(no analysis output)")

    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
