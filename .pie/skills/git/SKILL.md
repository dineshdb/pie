---
name: git
description: Advanced Git manipulation - surgery, recovery, rewriting history, and diagnostic commands
---

## Git Philosophy

Treat history as a living document that can be groomed for clarity before publication. Use atomic commits and descriptive messages.

---

## Surgery & History Rewriting

### Interactive Rebase
Groom your commits before merging.

```bash
git rebase -i HEAD~n          # Edit, squash, fixup, or drop last n commits
git rebase -i <base-branch>   # Rebase current branch on top of base interactively
```

### Amending
Fix the last commit without creating a new one.

```bash
git add .
git commit --amend --no-edit  # Add changes to the last commit
git commit --amend -m "new message" # Change last commit message
```

### Cherry-picking
Apply a specific commit from another branch.

```bash
git cherry-pick <commit-hash>
git cherry-pick <start-hash>^..<end-hash> # Range of commits
```

### Partial Commits
Commit only parts of a file.

```bash
git add -p <file>             # Interactively choose hunks
```

---

## Recovery & Undo

### Reflog
The ultimate "undo" button for Git. Finds commits that are not reachable by any branch or tag.

```bash
git reflog                    # View history of HEAD movements
git reset --hard HEAD@{n}     # Move back to a state seen in reflog
```

### Unstaging & Resetting
```bash
git restore --staged <file>   # Unstage a file
git restore <file>            # Discard local changes in a file
git reset --soft HEAD~1       # Undo last commit, keep changes staged
git reset --mixed HEAD~1      # Undo last commit, keep changes unstaged
git reset --hard HEAD~1       # Undo last commit, DISCARD all changes
```

### Stashing
Temporary storage for dirty state.

```bash
git stash push -m "work in progress"
git stash list
git stash apply stash@{0}     # Apply but keep in stash
git stash pop                 # Apply and remove from stash
git stash show -p stash@{0}   # View changes in stash
```

---

## Diagnostics & Inspection

### Searching
```bash
git grep "pattern"            # Search in tracked files
git log -S "string"           # Search commit history for changes to a string (Pickaxe)
git log -G "regex"            # Search commit history for lines matching regex
git log -L :<funcname>:<file> # Trace evolution of a specific function
```

### Visualizing
```bash
git log --oneline --graph --all --decorate
git diff --stat <commit1>..<commit2>
git show --name-only <commit>
```

### Blame & Evolution
```bash
git blame -L 10,20 <file>     # See who changed lines 10-20
git log --follow <file>       # Follow file history across renames
```

---

## Clean Up

```bash
git clean -fd                 # Remove untracked files and directories
git branch -d <branch>        # Delete merged branch
git branch -D <branch>        # Force delete unmerged branch
git remote prune origin       # Remove stale remote-tracking branches
```

---

## Workflows

### Feature Branch Sync
Keep your feature branch up to date with `main` using rebase to maintain a linear history.

```bash
git checkout feature
git fetch origin
git rebase origin/main
# Resolve conflicts if any, then:
git add <resolved-files>
git rebase --continue
```

### Bisect
Find the commit that introduced a bug.

```bash
git bisect start
git bisect bad                # Current version is broken
git bisect good <commit-hash> # Last known working commit
# Git will checkout a middle commit. Test it:
# If broken: git bisect bad
# If working: git bisect good
# Repeat until the culprit is found.
git bisect reset              # Return to original state
```

### Worktrees
Work on multiple branches simultaneously without multiple clones.

```bash
git worktree add ../feature-fix feature-branch
# Now you have a separate directory for that branch
git worktree list
git worktree remove <name>
```
