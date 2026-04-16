When it seems like I'm talking about or giving instructions to claude, I'm
actually talking about this ai agent I'm building: pie. Work on local context.
Don't assume and rewrite the global prompt. I'm talking about this project.

## Testing

After each change, run following tests to verify if the change is valid.

- repo test
- test.py

## Rules

- You are not allowed to change tests just to make tests pass
- Tests should check the behavior of the program (specs) instead of
  implementation details
- Always use /simplify at the end of a feature request to find and fix any
  redundancies, extra code, unneeded features, etc.
- You should always rethink the available codebase in terms of new feature being
  added. Identify how it diverges, identifying places to trim, changes in
  architecture and organization to slim down and /simplify the codebase to keep
  it lean and clean.
