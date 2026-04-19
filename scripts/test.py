#!/usr/bin/env uv run
# /// script
# requires-python = ">=3.9"
# dependencies = ["pyyaml", "python-dotenv"]
# ///
"""YAML-driven integration tests for pie.

Runs all tests with debug tracing, then analyzes tool-call efficiency
with a single LLM call.

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
2. UNNECESSARY AGENT SPAWNS: Subagent used when direct tool call would suffice (rule: if only one subagent would be spawned, do it yourself)
3. FAILED RETRIES: Tool calls that failed and had to be retried due to bad command formatting
4. EXCESSIVE BACK-AND-FORTH: More LLM round-trips than needed for the task
5. MISSED PRELOADING: Skills/agents mentioned in the query that weren't pre-loaded, requiring extra tool calls to load them

For each issue found, report:
- What happened (with the specific tool call)
- Why it's inefficient
- Suggested fix

Also report a summary:
- Total LLM calls (count "raw model response" lines)
- Total tool calls (count "shell_tool", "subagent", "load_skills", "load_references")
- Efficiency score: 1-5 (5 = optimal, 1 = very wasteful)

Be specific. Reference actual commands and line content from the log. Skip a section if no issues are found."""

MAX_VALUE_LEN = 200


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


def check_online():
    try:
        urllib.request.urlopen("http://127.0.0.1:8000/v1/models", timeout=2)
        return True
    except urllib.error.HTTPError:
        return True
    except Exception:
        return False


def truncate_value(s, max_len=MAX_VALUE_LEN):
    s = s.strip()
    if len(s) <= max_len:
        return s
    return s[:max_len] + f"... ({len(s)} chars total)"


def run_pie(args, input_text=None, timeout=30):
    cmd = PIE + args
    if input_text:
        cmd += [input_text]
    cmd.append("--debug")
    cmd.append("--md")
    try:
        r = subprocess.run(
            cmd, capture_output=True, text=True, timeout=timeout, cwd=ROOT
        )
        return r.stdout + r.stderr, r.returncode
    except subprocess.TimeoutExpired:
        return "(timeout)", -1


def extract_trace(raw_output):
    """Extract concise tool-call trace from debug output, truncating verbose values."""
    lines = []
    for line in raw_output.splitlines():
        clean = re.sub(r"\x1b\[[0-9;]*m", "", line).strip()

        if "raw model response text" in clean:
            m = re.search(r"text=(.+)$", clean)
            text = m.group(1).strip() if m else "(empty)"
            lines.append(f"  LLM response: {truncate_value(text, 120)}")

        elif "raw model tool call" in clean:
            m = re.search(r"tool=(\S+)\s+input=(.+)$", clean)
            if m:
                tool_name = m.group(1)
                tool_input = truncate_value(m.group(2).strip())
                lines.append(f"  TOOL CALL: {tool_name} | {tool_input}")

        elif clean.startswith("shell:") and "cmd=" in clean:
            m = re.search(r"cmd=(.+)$", clean)
            if m:
                lines.append(f"  shell cmd: {m.group(1).strip()}")

        elif "shell:" in clean and "exit_code=" in clean:
            m = re.search(r"exit_code=(\d+)\s+stdout_len=(\d+)", clean)
            if m:
                code, size = m.group(1), m.group(2)
                status = "OK" if code == "0" else f"FAIL({code})"
                lines.append(f"  shell result: {status}, {size} chars")

        elif "subagent" in clean and ("name=" in clean or "done" in clean):
            truncated = re.sub(r"(text=|sys=).{100,}", r"\1...(truncated)", clean)
            lines.append(f"  {truncated}")

        elif "load_skills" in clean and "already loaded" in clean:
            lines.append("  load_skills: already loaded (skipped)")

    return "\n".join(lines)


def run_test(test):
    name = test["name"]
    max_retries = 3 if test.get("skip") == "online" else 1

    for attempt in range(max_retries):
        failures = []

        args = test.get("args", "").split() if test.get("args") else []
        out, exit_code = run_pie(
            args,
            input_text=test.get("input"),
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
                    preview = (r.stdout + r.stderr).strip().splitlines()[:3]
                    failures.append(f"post: {post_cmd!r} exited {r.returncode}")
                    for line in preview:
                        failures.append(f"  {line}")
            except subprocess.TimeoutExpired:
                failures.append(f"post: {post_cmd!r} timed out")

        if not failures:
            break

    trace = extract_trace(out)
    status = "fail" if failures else "pass"
    return name, status, failures, trace


def print_result(name, status, failures):
    if status == "skip":
        yellow(f"  SKIP: {name}")
    elif status == "fail":
        red(f"  FAIL: {name}")
        for f in failures:
            red(f"    - {f}")
    else:
        green(f"  PASS: {name}")


def analyze_traces(traces):
    """Single direct LLM call to analyze all test traces."""
    combined = []
    for name, trace in traces:
        combined.append(f"=== TEST: {name} ===\n{trace}\n")
    full_log = "\n".join(combined)

    base_url = (
        os.environ.get("OPENAI_BASE_URL")
        or os.environ.get("OPENAI_BASE_URL", "http://127.0.0.1:8000/v1")
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
        return f"(analysis failed: HTTP {e.code} — {body})"
    except Exception as e:
        return f"(analysis failed: {e})"


def main():
    load_dotenv(ROOT / ".env")

    print("Building pie...")
    subprocess.run(["cargo", "build", "--quiet"], cwd=ROOT, check=True)
    print()

    with open(ROOT / "tests" / "tests.yaml") as f:
        tests = yaml.safe_load(f)["tests"]

    print("══ Running tests ══\n")

    traces = []
    passed = failed = skipped = 0

    for test in tests:
        name, status, failures, trace = run_test(test)
        print_result(name, status, failures)
        if status == "pass":
            passed += 1
        elif status == "fail":
            failed += 1
        else:
            skipped += 1
        if status != "skip" and trace:
            traces.append((name, trace))

    print("\n══ Results ══")
    green(f"  Passed: {passed}")
    if failed:
        red(f"  Failed: {failed}")
    else:
        print(f"  Failed: {failed}")
    if skipped:
        yellow(f"  Skipped: {skipped}")
    else:
        print(f"  Skipped: {skipped}")

    if traces:
        print("\n══ Efficiency Analysis ══\n")
        analysis = analyze_traces(traces)
        if analysis:
            cyan(analysis)
        else:
            yellow("(no analysis output)")

    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
