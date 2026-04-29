---
name: filesystem
description: File operations, surgical edits, and codebase search.
---

# Filesystem Operations

Precise primitives for exploring and modifying the codebase.

## 1. Inspection & Search

### Read with Line Numbers
Always use line numbers to identify targets for edits.
```bash
cat -n <file>
sed -n '<start>,<end>p' <file> | cat -n # Range
```

### Map Structure
```bash
find . -maxdepth 3 -not -path '*/.*'    # Quick map
ls -R | grep ":$" | sed -e 's/:$//' -e 's/[^-][^\/]*\//--/g' -e 's/^/   /' # Tree
```
Trying to find file tree without depth and filtering out large modules like `.git`, `node_modules`, `target` and other project dependencies will eat your context. Filter those out.

### Search Content (Grep)
```bash
grep -rnE '<regex>' .                   # Recursive regex
grep -rnC 3 '<pattern>' .               # With context
grep -rl '<pattern>' .                  # List files only
```

## 2. Modification Primitives

### Surgical Edits (Python)
The most reliable method for multiline or complex replacements. Ensures uniqueness.
```bash
python3 << 'PYEOF'
path, old, new = "file.rs", """old""", """new"""
with open(path) as f: content = f.read()
if content.count(old) != 1: print("Uniqueness Error"); exit(1)
with open(path, "w") as f: f.write(content.replace(old, new, 1))
PYEOF
```

### File Creation
```bash
cat > <path> << 'EOF'
<content>
EOF
```

## Safety Mandates

1. **Read-Before-Write**: Never edit a file without reading it first.
2. **Verify-After-Edit**: Immediately `cat` or `grep` the change to confirm.
3. **Atomic Edits**: One logical change per operation.
4. **Path Safety**: Always `mkdir -p` before creating files in new directories.
