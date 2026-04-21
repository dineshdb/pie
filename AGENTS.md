When it seems like I'm talking about or giving instructions to claude, I'm
actually talking about this ai agent I'm building: pie. Work on local context.
Don't assume and rewrite the global prompt. I'm talking about this project.

## Testing

After each change, run following tests to verify if the change is valid.

- repo test
- test.py gives a summary of issues in the codebase based on runtime behavior.
  You should review the response and try to fix the issues.
- Deterministic tests go to the rust tests, non deterministic tests go to
  tests.yaml.
- You are not allowed to change tests just to make tests pass
- Tests should check the behavior of the program (specs) instead of
  implementation details

## Architecture

- You should always rethink the available codebase in terms of new feature being
  added. Identify how it diverges, identifying places to trim, changes in
  architecture and organization to slim down and /simplify the codebase to keep
  it lean and clean.

## Simplification

- follow rust 2024 ergonomics
- use early return patterns and other patterns for simpler logic
- try to use dry principle but not always.
- use From impl instead of from_ to_ methods.
- Try to reduce copies for simple tasks, use &'str and other references.
  However, don't complicate structs with references. Instead, opt to rearchitect
  the problem in a way copies are unnecessary. Hexagonal architecture, MVU
  patterns, etc help with this.
