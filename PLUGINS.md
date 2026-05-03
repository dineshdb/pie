# Pie Plugin System

Pie supports a powerful, folder-based plugin system for **Unconditional Steering**. Hooks allow you to intercept and modify the agent's behavior at various lifecycle events.

## Directory Structure

Plugins are located in `.pie/plugins/` (project-local) or `~/.pie/plugins/` (global).

```text
.pie/plugins/
└── <plugin-name>/
    ├── plugin.toml       # Manifest file
    ├── SYSTEM.md         # (Optional) Static system prompt instructions
    └── bin/              # Executable scripts
        └── <hook-script>
```

## Plugin Manifest (`plugin.toml`)
...
## SYSTEM.md

If a plugin contains a `SYSTEM.md` file, its content will be automatically injected into the agent's system prompt, prefixed with the plugin name. This is the preferred way to add **Static Instructions** to the agent.

The manifest defines the hooks and their configuration.

```toml
name = "my-plugin"
version = "0.1.0"
description = "My custom steering hooks"

[[hooks]]
name = "my-hook"
event = "tool.pre"      # Event to trigger on
type = "cmd"            # Type (always "cmd" for now)
handler = "script.py"   # Name of the script in bin/
scope = "validation"    # "validation" or "transform"
on_failure = "warn"     # "abort", "warn", or "continue"
matcher = { tools = ["shell"] } # Optional tool filter
```

## Hook Lifecycle Events

| Event | Description |
| :--- | :--- |
| `prompt.pre` | Fires before the system prompt is rendered. Used for grounding. |
| `tool.pre` | Fires before a tool is executed. Used for safety/validation. |
| `tool.post` | Fires after a tool executes. Used for verification/feedback. |

## Execution Environment

Hooks are executed as shell commands with the following context:

- **PATH**: The plugin's `bin/` directory is prepended to `PATH`.
- **PWD**: The current project working directory.
- **Environment Variables**:
  - `PIE_EVENT`: The current event (e.g., `tool.pre`).
  - `PIE_HOOK_NAME`: The name of the hook.
  - `PIE_CWD`: The current working directory.
  - `PIE_SESSION_ID`: Unique ID for the current session.
  - `PIE_PLUGIN_DIR`: Absolute path to the plugin's directory.
  - `PIE_INPUT`: JSON string of the context data (also available via `stdin`).

## Scopes & Control Flow

### Validation Scope
Used to verify if an action should proceed.
- **Exit Code 0**: Success.
- **Exit Code 1**: Warning (if `on_failure = "warn"`). Pie prepends `stderr` to the tool output.
- **Exit Code 2 (or 64, 65, 77)**: Abort/Block. Pie stops execution and feeds `stderr` back to the agent.

### Transform Scope
Used to modify data (system prompt, tool input, or tool output).
- The script must print a JSON object to `stdout` containing the modified fields (e.g., `{"system": "new prompt"}` or `{"output": "new output"}`).

## Best Practices
- **Zero Dependencies**: Prefer `uv run` with inline dependencies for Python scripts to ensure portability.
- **Surgical Feedback**: Use Markdown in `stderr` for high-quality agent feedback.
- **Non-Blocking**: Use `validation` with `on_failure = "warn"` for quality checks that shouldn't stop the flow.
