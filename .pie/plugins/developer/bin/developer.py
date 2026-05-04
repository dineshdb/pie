#!/usr/bin/env uv run
# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
import hashlib
import json
import os
import re
import sqlite3
import subprocess
import sys
from datetime import datetime

# --- Utilities ---


def get_db():
    db_path = os.environ.get("PIE_DATABASE_PATH")
    if not db_path:
        return None
    return sqlite3.connect(db_path)


def read_input():
    input_json = os.environ.get("PIE_INPUT")
    if not input_json:
        input_json = sys.stdin.read()
    if not input_json:
        return {}
    return json.loads(input_json)


def find_upward(filename, start_dir):
    current = os.path.abspath(start_dir)
    found_paths = []
    while True:
        target = os.path.join(current, filename)
        if os.path.exists(target):
            found_paths.append(target)
        if (
            current == "/"
            or current == os.path.expanduser("~")
            or os.path.exists(os.path.join(current, ".git"))
        ):
            break
        new_current = os.path.dirname(current)
        if new_current == current:
            break
        current = new_current
    return found_paths


# --- Grounding Logic ---


def get_git_status():
    try:
        subprocess.run(
            ["git", "rev-parse", "--is-inside-work-tree"],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            check=True,
        )
        branch = (
            subprocess.check_output(["git", "rev-parse", "--abbrev-ref", "HEAD"])
            .decode()
            .strip()
        )
        status = subprocess.check_output(["git", "status", "--short"]).decode().strip()
        context = f"\n- **Branch**: `{branch}`"
        if status:
            context += f"\n- **Pending Changes**:\n```\n{status}\n```"
        else:
            context += "\n- **Status**: Clean"
        return context
    except:
        return ""


def get_recent_files():
    try:
        cmd = [
            "find",
            ".",
            "-maxdepth",
            "4",
            "-not",
            "-path",
            "*/.*",
            "-not",
            "-path",
            "./target/*",
            "-mtime",
            "-1",
            "-type",
            "f",
        ]
        files = (
            subprocess.check_output(cmd, stderr=subprocess.DEVNULL)
            .decode()
            .strip()
            .split("\n")
        )
        files = [f for f in files if f and not f.endswith((".swp", "~", ".lock"))]
        if files:
            files.sort(
                key=lambda x: os.path.getmtime(x) if os.path.exists(x) else 0,
                reverse=True,
            )
            return "\n" + "\n".join([f"- {f}" for f in files[:10]])
    except:
        pass
    return ""


def find_test_file(path):
    if not path or not os.path.exists(path):
        return None
    base = os.path.basename(path)
    name, ext = os.path.splitext(base)
    dirname = os.path.dirname(path)
    candidates = [
        os.path.join(dirname, f"{name}_test{ext}"),
        os.path.join(dirname, "tests.rs"),
        os.path.join(dirname, "test.rs"),
    ]
    root = dirname
    while root != "/" and not os.path.exists(os.path.join(root, ".git")):
        new_root = os.path.dirname(root)
        if new_root == root:
            break
        root = new_root
    if os.path.exists(os.path.join(root, "tests")):
        candidates.extend(
            [
                os.path.join(root, "tests", f"{name}{ext}"),
                os.path.join(root, "tests", f"{name}_test{ext}"),
                os.path.join(root, "tests", "mod.rs"),
            ]
        )
    for c in candidates:
        if os.path.exists(c):
            return c
    return None


def get_test_mirror():
    try:
        cmd = [
            "find",
            ".",
            "-maxdepth",
            "4",
            "-not",
            "-path",
            "*/.*",
            "-mmin",
            "-120",
            "-type",
            "f",
        ]
        recent = (
            subprocess.check_output(cmd, stderr=subprocess.DEVNULL)
            .decode()
            .strip()
            .split("\n")
        )
        recent = [
            f
            for f in recent
            if f
            and not "test" in f.lower()
            and not f.endswith((".md", ".toml", ".lock"))
        ]
        test_map = []
        visited = set()
        for f in recent[:5]:
            test = find_test_file(f)
            if test and test not in visited:
                test_map.append(f"- **{f}** tests in `{test}`")
                visited.add(test)
        return "\n" + "\n".join(test_map) if test_map else ""
    except:
        return ""


def get_stack_info():
    info = ""
    if os.path.exists("Cargo.toml"):
        info += "\n- **Rust**: `just build/test`, `cargo check`"
    if os.path.exists("package.json"):
        try:
            with open("package.json", "r") as f:
                scripts = json.load(f).get("scripts", {})
                if scripts:
                    info += "\n- **Node**: " + ", ".join(
                        [f"`npm run {s}`" for s in list(scripts.keys())[:3]]
                    )
        except:
            pass
    if os.path.exists("Justfile"):
        info += "\n- **Just**: Use `just <task>`"
    return info


def get_docs():
    docs = []
    for f in ["ARCHITECTURE.md", "DESIGN.md", "CONTRIBUTING.md", "API.md"]:
        for p in [f, os.path.join("docs", f)]:
            if os.path.exists(p):
                docs.append(f"- **{f}** at `{p}`")
                break
    return "\n" + "\n".join(docs) if docs else ""


def get_repo_development_prompts():
    try:
        subprocess.run(
            ["git", "rev-parse", "--is-inside-work-tree"],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            check=True,
        )
        return """
### Development Guidelines (Repository Detected)

1. **Verify Context**: Always run `repo context` to understand the project structure before starting.
2. **Test-Driven**: Look for existing tests. If adding a feature, add a test first. If fixing a bug, reproduce it with a test.
3. **Atomic Changes**: Keep your changes focused and atomic.
4. **Style Alignment**: Adhere to the project's existing coding style and conventions.
5. **Documentation**: Update relevant documentation (README, API docs) if your changes affect them.
"""
    except:
        return ""


# --- Command Handlers ---


def cmd_ground(args, data):
    new_content = ""
    sections = [
        ("Git Context", get_git_status()),
        ("Recently Modified", get_recent_files()),
        ("Related Tests", get_test_mirror()),
        ("Tech Stack", get_stack_info()),
        ("Key Documentation", get_docs()),
        ("Development Strategy", get_repo_development_prompts()),
    ]
    for title, content in sections:
        if content.strip():
            new_content += f"\n\n### {title}\n{content.strip()}"
    print(json.dumps({"system": new_content}))


def cmd_memory_ground(args, data):
    new_content = ""
    cwd = os.environ.get("PIE_CWD", os.getcwd())

    # Global Personal Memory
    global_gemini = os.path.expanduser("~/.gemini/GEMINI.md")
    if os.path.exists(global_gemini):
        with open(global_gemini, "r") as f:
            new_content += f"\n\n### Global Personal Memory\n{f.read()}"

    # Private Project Memory
    private_memory = os.path.expanduser("~/.gemini/tmp/pie/memory/MEMORY.md")
    if os.path.exists(private_memory):
        with open(private_memory, "r") as f:
            new_content += f"\n\n### Private Project Memory\n{f.read()}"

    # Project & Subdirectory Instructions
    instructions = find_upward("GEMINI.md", cwd)
    if instructions:
        new_content += "\n\n## Project Instructions (GEMINI.md)"
        for path in reversed(instructions):
            with open(path, "r") as f:
                rel_path = os.path.relpath(path, cwd)
                new_content += f"\n\n### From {rel_path}:\n{f.read()}"

    print(json.dumps({"system": new_content}))


def cmd_guard(args, data):
    tool = data.get("tool")
    input_data = data.get("input", {})
    session_id = os.environ.get("PIE_SESSION_ID", "default")

    # 1. Redundancy check
    db = get_db()
    if db:
        input_str = json.dumps(input_data, sort_keys=True)
        hash_id = hashlib.md5((tool + input_str).encode()).hexdigest()
        cursor = db.cursor()
        cursor.execute(
            "SELECT hash FROM messages WHERE session_id = ? AND role = 'tool' ORDER BY ts DESC LIMIT 5",
            (session_id,),
        )
        recent = cursor.fetchall()
        if recent:
            if recent[0][0] == hash_id:
                print(
                    f"### [Safety/Efficiency Alert]\nEFFICIENCY: You just ran this `{tool}` call in the previous turn. STOP and re-evaluate.",
                    file=sys.stderr,
                )
                sys.exit(2)
            elif any(r[0] == hash_id for r in recent):
                print(
                    f"### [Safety/Efficiency Alert]\nEFFICIENCY: You ran this `{tool}` call recently. Ensure you aren't stuck in a loop.",
                    file=sys.stderr,
                )
        db.close()

    # 2. Safety (Shell)
    if tool == "shell":
        cmd = input_data.get("cmd", "")
        for pattern in [
            "rm -rf /",
            "rm -rf .git",
            "rm -rf *",
            "chmod 777 /",
            ":(){ :|:& };:",
        ]:
            if pattern in cmd:
                print(
                    f"### [Safety Alert]\nCRITICAL: Destructive pattern `{pattern}` blocked.",
                    file=sys.stderr,
                )
                sys.exit(2)

    # 3. Security
    if tool in ["write_file", "replace"]:
        content = input_data.get("content", "") or input_data.get("new_string", "")
        for pattern in [
            r"(?i)api[-_]?key",
            r"(?i)secret",
            r"(?i)token",
            r"sk-[a-zA-Z0-9]{20,}",
            r"AIza[0-9A-Za-z-_]{35}",
        ]:
            if re.search(pattern, content):
                print(
                    f"### [Security Alert]\nSECURITY: Potential secret or API key detected.",
                    file=sys.stderr,
                )
                sys.exit(2)

    # 4. Optimization
    if tool == "read_file" and input_data.get("start_line") is None:
        path = input_data.get("path", "")
        if path and os.path.exists(path) and os.path.getsize(path) > 50 * 1024:
            print(
                f"### [Optimization Alert]\nOPTIMIZATION: `{path}` is large. Use `start_line` or `grep_search`.",
                file=sys.stderr,
            )
            sys.exit(2)

    sys.exit(0)


def cmd_step_alignment(args, data):
    new_content = ""
    session_id = os.environ.get("PIE_SESSION_ID", "default")
    db = get_db()
    if db:
        cursor = db.cursor()
        cursor.execute(
            "SELECT count(*) FROM messages WHERE session_id = ? AND ts > (SELECT COALESCE(max(ts), 0) FROM messages WHERE session_id = ? AND content LIKE '%task_add%' OR content LIKE '%task_update%')",
            (session_id, session_id),
        )
        turns = cursor.fetchone()[0]
        if turns >= 3:
            new_content = f"\n\n### [Task Alignment Alert]\nYou have taken **{turns} turns** without updating your task list. \n**Mandate**: Ensure your plan is still accurate."
        db.close()
    print(json.dumps({"system": new_content}))


def cmd_step_integrity(args, data):
    tool = data.get("tool")
    input_data = data.get("input", {})
    session_id = os.environ.get("PIE_SESSION_ID", "default")
    if tool == "plan_step_update":
        if any(u.get("status") == "completed" for u in input_data.get("updates", [])):
            db = get_db()
            if db:
                cursor = db.cursor()
                cursor.execute(
                    "SELECT tool FROM tool_calls WHERE session_id = ? ORDER BY ts DESC LIMIT 3",
                    (session_id,),
                )
                recent = [r[0] for r in cursor.fetchall()]
                if not any(t in ["write_file", "replace", "shell"] for t in recent):
                    print(
                        f"### [Step Integrity Warning]\nYou are marking steps as `completed`, but your recent actions were purely explorative.",
                        file=sys.stderr,
                    )
                    sys.exit(1)
                db.close()
    sys.exit(0)


def cmd_test_first(args, data):
    session_id = os.environ.get("PIE_SESSION_ID", "default")
    db = get_db()
    if db:
        cursor = db.cursor()
        cursor.execute(
            "SELECT count(*) FROM messages WHERE session_id = ? AND (content LIKE '%test%' OR content LIKE '%check%' OR content LIKE '%just%' OR content LIKE '%repo%')",
            (session_id,),
        )
        if cursor.fetchone()[0] == 0:
            print(
                "### [Quality Alert]\nYou are attempting to modify code without having read or run any tests in this session.",
                file=sys.stderr,
            )
            sys.exit(1)
        db.close()
    sys.exit(0)


def cmd_build_check(args, data):
    def run_check(cmd, label):
        try:
            p = subprocess.run(cmd, capture_output=True, text=True, timeout=30)
            if p.returncode != 0:
                print(
                    f"\n### [Build Guard Failure] {label}\n```\n{p.stdout}\n{p.stderr}\n```",
                    file=sys.stderr,
                )
                sys.exit(1)
        except:
            pass

    if os.path.exists("Cargo.toml"):
        run_check(["cargo", "check", "--color", "never"], "Rust")
    elif os.path.exists("package.json"):
        run_check(["npm", "run", "lint"], "Node.js")
    sys.exit(0)


def cmd_diff_sentinel(args, data):
    tool = data.get("tool")
    input_data = data.get("input", {})
    output = data.get("output", "")
    path = input_data.get("path")
    if tool in ["write_file", "replace"] and path:
        try:
            diff = (
                subprocess.check_output(
                    ["git", "diff", path], stderr=subprocess.DEVNULL
                )
                .decode()
                .strip()
            )
            if diff:
                output = (
                    str(output)
                    + f"\n\n### [Auto-Verification] Git Diff\n```diff\n{diff}\n```"
                )
        except:
            pass
    print(json.dumps({"output": output}))


def cmd_doom_loop(args, data):
    tool = data.get("tool")
    output = data.get("output", "")
    session_id = os.environ.get("PIE_SESSION_ID", "default")
    if tool in ["shell", "build_check"] and (
        "Failure" in str(output) or "error" in str(output).lower()
    ):
        db = get_db()
        if db:
            cursor = db.cursor()
            curr_hash = hashlib.md5(str(output).encode()).hexdigest()
            cursor.execute(
                "SELECT turn_id FROM tool_calls WHERE session_id = ? ORDER BY ts DESC LIMIT 1",
                (session_id,),
            )
            row = cursor.fetchone()
            if row:
                turn_id = row[0]
                cursor.execute(
                    "UPDATE tool_calls SET fail_hash = ? WHERE session_id = ? AND turn_id = ?",
                    (curr_hash, session_id, turn_id),
                )
                cursor.execute(
                    "SELECT count(*) FROM tool_calls WHERE session_id = ? AND turn_id < ? AND fail_hash = ?",
                    (session_id, turn_id, curr_hash),
                )
                if cursor.fetchone()[0] > 0:
                    print(
                        f"### [Doom Loop Detected]\nIdentical failure to a previous turn. Re-evaluate strategy.",
                        file=sys.stderr,
                    )
            db.commit()
            db.close()
    sys.exit(0)


def cmd_diagnostic(args, data):
    tool = data.get("tool")
    output = data.get("output", "")
    if tool == "shell":
        output_str = str(output)
        enrich = ""
        if "error[E" in output_str:
            enrich = "\n\n### [Diagnostic] Rust Compiler Error Detected\n- Try `rustc --explain <error_code>`."
        elif "ModuleNotFoundError" in output_str:
            enrich = "\n\n### [Diagnostic] Python Import Error Detected."
        if enrich:
            print(json.dumps({"output": output + enrich}))
            return
    sys.exit(0)


# --- Main ---


def main():
    if len(sys.argv) < 2:
        print("Usage: developer.py <command>")
        sys.exit(1)

    cmd = sys.argv[1]
    data = read_input()

    handlers = {
        "ground": cmd_ground,
        "memory-ground": cmd_memory_ground,
        "guard": cmd_guard,
        "step-alignment": cmd_step_alignment,
        "step-integrity": cmd_step_integrity,
        "test-first": cmd_test_first,
        "build-check": cmd_build_check,
        "diff-sentinel": cmd_diff_sentinel,
        "doom-loop": cmd_doom_loop,
        "diagnostic": cmd_diagnostic,
    }

    if cmd in handlers:
        handlers[cmd](sys.argv[2:], data)
    else:
        print(f"Unknown command: {cmd}")
        sys.exit(1)


if __name__ == "__main__":
    main()
