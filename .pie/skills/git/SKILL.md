---
name: git
description: Advanced history manipulation, recovery, and diagnostics.
---

# Advanced Git Operations

Treat history as a document to be groomed for clarity.

## 1. History Surgery
```bash
git rebase -i HEAD~n          # Groom last n commits (squash/edit/drop)
git commit --amend --no-edit  # Add staged changes to last commit
git cherry-pick <hash>        # Apply specific commit to current branch
```

## 2. Recovery & Undo
```bash
git reflog                    # Find "lost" commits (HEAD history)
git reset --hard HEAD@{n}     # Restore to a specific reflog state
git reset --soft HEAD~1       # Undo commit, keep changes staged
git restore <file>            # Discard local changes
```

## 3. Diagnostics & Trace
```bash
git log -S "string"           # Find when a string was added/removed (Pickaxe)
git log -L :<func>:<file>      # Evolution of a specific function
git blame -L 10,20 <file>     # Line-level authorship
git show --name-only <hash>   # List files changed in a commit
```

## 4. Maintenance
```bash
git stash push -m "msg"       # Save dirty state
git stash pop                 # Restore dirty state
git clean -fd                 # Remove untracked files/dirs
git worktree add ../path br   # Work on another branch simultaneously
```

## Principles
- **Atomic Commits**: One logical change per commit.
- **Linear History**: Prefer rebase over merge for feature updates.
- **Descriptive Messages**: Focus on "why" more than "what".
