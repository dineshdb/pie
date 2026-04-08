---
name: filesystem
description: Comprehensive file operations: read, write, edit, find, and search files via shell commands
---

# Filesystem Operations

You manipulate files exclusively through `shell_tool`. Follow these patterns to
read, write, edit, find, and search files reliably.

## Core Principle

**ALWAYS read a file before editing it.** Understand existing content before
making changes. **Verify after every edit** by reading the affected lines.

---

## 1. Reading Files

### Read entire file with line numbers

```bash
cat -n <file>
```

Each line is prefixed with its line number. Use this to identify exact line
numbers for targeted edits.

### Read a line range

```bash
sed -n '<start>,<end>p' <file> | cat -n
```

Example: `sed -n '10,25p' src/main.rs | cat -n` reads lines 10-25 with relative
numbering.

### Read first N lines

```bash
head -n <N> <file>
```

### Read last N lines

```bash
tail -n <N> <file>
```

### Read last N lines with line numbers

```bash
tail -n <N> <file> | cat -n
```

### Check if file exists

```bash
test -f <file> && echo "EXISTS" || echo "NOT FOUND"
```

### File metadata

```bash
wc -l <file>       # line count
wc -c <file>       # byte count
file <file>        # file type detection
stat <file>        # detailed metadata
```

### Detect file encoding

```bash
file -I <file>
```

---

## 2. Writing Files

### Create or overwrite a file (full content)

```bash
cat > <filepath> << 'ENDOFFILE'
<content here>
ENDOFFILE
```

**Critical rules:**

- Always single-quote the delimiter: `<< 'DELIM'` not `<< DELIM` -- this
  prevents shell variable expansion inside the heredoc
- The delimiter line must be alone on its line with no trailing whitespace
- If the content itself contains `ENDOFFILE`, choose a different delimiter
  string
- Create parent directories first if needed: `mkdir -p $(dirname <filepath>)`

### Append to an existing file

```bash
cat >> <filepath> << 'ENDOFFILE'
<content to append>
ENDOFFILE
```

### Write from a variable or pipeline

```bash
echo '<content>' > <file>           # Single line only
printf '%s\n' '<content>' > <file>  # Safer for special chars
some_command > <file>                # Redirect command output
some_command >> <file>               # Append command output
```

---

## 3. Editing Files

Editing is the most critical and error-prone operation. Follow these rules
strictly.

### Rules

1. **Read the file first** -- always `cat -n <file>` before editing
2. **Make targeted edits** -- replace only what needs changing
3. **Ensure target uniqueness** -- the text you're replacing must appear exactly
   once
4. **Verify after editing** -- read the modified section to confirm correctness
5. **Match existing style** -- preserve indentation, spacing, and formatting
   exactly

### Method 1: Python replacement (RECOMMENDED)

Python handles special characters, newlines, and complex replacements reliably.
Use this for all non-trivial edits.

**Single replacement (fails if not unique):**

```bash
python3 << 'PYEOF'
path = "src/main.rs"
with open(path) as f:
    content = f.read()

old = """exact old string
possibly multiline"""

new = """exact new string
possibly multiline"""

count = content.count(old)
if count == 0:
    print(f"ERROR: old_string not found in {path}")
    exit(1)
if count > 1:
    print(f"ERROR: old_string matches {count} locations, must be unique. Add more surrounding context.")
    exit(1)

content = content.replace(old, new, 1)
with open(path, "w") as f:
    f.write(content)
print(f"OK: replaced 1 occurrence in {path}")
PYEOF
```

**Replace all occurrences:**

```bash
python3 << 'PYEOF'
path = "src/main.rs"
with open(path) as f:
    content = f.read()

old = "old_string"
new = "new_string"

count = content.count(old)
content = content.replace(old, new)
with open(path, "w") as f:
    f.write(content)
print(f"OK: replaced {count} occurrences in {path}")
PYEOF
```

### Method 2: sed for simple single-line substitutions

Use sed ONLY for simple substitutions on single lines with no special
characters.

```bash
# macOS (note the empty string after -i)
sed -i '' 's|old_text|new_text|' <file>       # First occurrence per line
sed -i '' 's|old_text|new_text|g' <file>      # All occurrences

# Linux
sed -i 's|old_text|new_text|' <file>
```

**sed limitations:**

- Fails with special regex characters in the pattern: `.`, `*`, `[`, `\`, `$`,
  `^`, `&`
- Use `|` as delimiter to avoid conflicts with `/` in paths
- Does NOT handle multiline replacements
- Prefer Python for anything beyond trivial single-line substitutions

### Method 3: Delete specific lines by line number

```bash
# Delete lines 10-15
sed -i '' '10,15d' <file>

# Delete lines matching a pattern
sed -i '' '/pattern/d' <file>
```

### Method 4: Insert lines

```bash
# Insert after line 10
sed -i '' '10a\
new line here' <file>

# Insert before line 10
sed -i '' '10i\
new line here' <file>

# Insert after a pattern match
sed -i '' '/pattern/a\
new line' <file>
```

### Edit Verification

After EVERY edit, verify the result:

```bash
# Read the modified area
sed -n '<start>,<end>p' <file> | cat -n

# Or read the whole file if it's small
cat -n <file>
```

---

## 4. Finding Files (Glob Patterns)

### Find files by name pattern

```bash
find <dir> -name '<pattern>' -type f 2>/dev/null
```

Examples:

```bash
find src -name '*.rs' -type f             # All Rust files in src/
find . -name '*.toml' -type f             # All TOML files
find . -name 'Cargo.toml' -type f         # All Cargo.toml files
find src -path '*/core/*.rs' -type f      # Rust files in any core/ subdirectory
find . -name '*.test.*' -type f           # Test files
```

### Find directories

```bash
find <dir> -name '<pattern>' -type d 2>/dev/null
find . -maxdepth 2 -type d                # List dirs up to depth 2
```

### Find files by multiple extensions

```bash
find . -type f \( -name '*.rs' -o -name '*.toml' \)
```

### Find files excluding directories

```bash
find . -name '*.rs' -not -path '*/target/*' -not -path '*/.git/*'
```

### Find recently modified files

```bash
find <dir> -type f -mtime -1              # Modified in last 24 hours
find <dir> -type f -newer <reference_file> # Newer than reference
```

### Find files sorted by modification time

```bash
find <dir> -type f -exec stat -f '%m %N' {} \; 2>/dev/null | sort -rn | head -20
# On Linux, use: stat -c '%Y %n'
```

### List directory contents

```bash
ls -la <dir>                              # Detailed listing
ls -la                                     # Current directory
ls -la <dir> | grep '<pattern>'           # Filter by pattern
```

### Directory tree structure

```bash
find <dir> -type f | head -100            # Limited file listing
find <dir> -type f -not -path '*/.*' | sort  # All files, excluding hidden
```

### Find empty directories

```bash
find <dir> -type d -empty
```

---

## 5. Searching Content (Grep)

### Basic recursive search

```bash
grep -rn '<pattern>' <dir>
```

### With context lines

```bash
grep -rn -C 3 '<pattern>' <dir>           # 3 lines before and after
grep -rn -B 2 '<pattern>' <dir>           # 2 lines before only
grep -rn -A 5 '<pattern>' <dir>           # 5 lines after only
```

### Regex search

```bash
grep -rnE '<regex>' <dir>
```

Examples:

```bash
grep -rnE 'fn \w+\(' src/                 # Function definitions
grep -rnE 'struct \w+' src/               # Struct definitions
grep -rnE 'impl \w+ for \w+' src/         # Trait implementations
grep -rnE 'use .*::\{' src/               # Grouped imports
grep -rnE '(TODO|FIXME|HACK)' src/        # Code annotations
```

### Only show filenames containing matches

```bash
grep -rl '<pattern>' <dir>
```

### Count matches per file

```bash
grep -rc '<pattern>' <dir> | grep -v ':0$'
```

### Case-insensitive search

```bash
grep -rni '<pattern>' <dir>
```

### Filter by file type/extension

```bash
grep -rn '<pattern>' --include='*.rs' <dir>
grep -rn '<pattern>' --include='*.{rs,toml}' <dir>
grep -rn '<pattern>' --exclude='*.lock' <dir>
grep -rn '<pattern>' --exclude-dir='target' <dir>
```

### Invert match (lines NOT matching)

```bash
grep -rn '<pattern>' <dir> -v
```

### Show only the matched part (no filename/line)

```bash
grep -rnho '<pattern>' <dir>
```

### Whole word match

```bash
grep -rnw '<word>' <dir>
```

### Multiline patterns

```bash
# Match pattern across multiple lines
grep -rnzP 'fn \w+\([^)]*\)[^{]*\{' <dir>
```

### Search with file type constraints

```bash
# Search only in tracked git files
git ls-files | xargs grep '<pattern>'

# Search only modified files
git diff --name-only | xargs grep '<pattern>'
```

---

## 6. File Operations

### Copy

```bash
cp <src> <dst>                            # Copy file
cp -r <src_dir> <dst_dir>                 # Copy directory recursively
cp -p <src> <dst>                         # Preserve permissions/timestamps
```

### Move / Rename

```bash
mv <src> <dst>
```

### Delete (use with extreme caution)

```bash
rm <file>                                 # Delete a file
rm -rf <dir>                              # Delete directory recursively (DANGEROUS)
```

**Safety:** Never run `rm -rf` without being certain of the path. Always verify
the path exists first.

### Create directories

```bash
mkdir -p <path>                           # Create with parent directories
```

### Permissions

```bash
chmod +x <file>                           # Make executable
chmod 644 <file>                          # Standard file permissions
chmod 755 <dir>                           # Standard directory permissions
```

### Symlinks

```bash
ln -s <target> <link_name>                # Create symbolic link
readlink <link>                           # Read link target
```

---

## 7. Special File Types

### JSON files

```bash
python3 -m json.tool <file.json>          # Pretty print
python3 -c "import json,sys; d=json.load(sys.stdin); print(json.dumps(d, indent=2))" < <file>
```

### YAML files

```bash
python3 -c "import yaml,json,sys; print(json.dumps(yaml.safe_load(sys.stdin), indent=2))" < <file>
```

### TOML files

```bash
python3 -c "import tomllib,json,sys; print(json.dumps(tomllib.load(sys.stdin.buffer), indent=2))" < <file>
```

### Compressed files

```bash
zcat <file.gz>                            # Read gzip
tar -tzf <file.tar.gz>                    # List tar.gz contents
tar -xzf <file.tar.gz>                    # Extract tar.gz
unzip -l <file.zip>                       # List zip contents
unzip <file.zip>                          # Extract zip
```

### Binary files

```bash
file <path>                               # Identify file type
xxd <path> | head -20                     # Hex dump
strings <path> | head -20                 # Extract printable strings
```

---

## 8. Diff and Comparison

### Compare files

```bash
diff <file1> <file2>                      # Unified diff
diff -u <file1> <file2>                   # Unified format
diff --side-by-side <file1> <file2>       # Side by side
```

### Compare directories

```bash
diff -rq <dir1> <dir2>                    # Quick: list differing files
diff -r <dir1> <dir2>                     # Full recursive diff
```

### Git diff

```bash
git diff                                  # Unstaged changes
git diff --cached                         # Staged changes
git diff HEAD                             # All changes vs HEAD
git diff <commit1>..<commit2>             # Between commits
git diff --stat                           # Summary only
```

---

## Safety Rules

1. **ALWAYS read before editing** -- `cat -n <file>` first
2. **Verify after editing** -- read the modified section to confirm correctness
3. **Ensure target uniqueness** -- old_string must match exactly one location
4. **Preserve formatting** -- match the existing indentation, spacing, and line
   endings exactly
5. **Backup critical files** -- for risky edits, copy first:
   `cp <file> <file>.bak`
6. **Never blindly overwrite** -- use targeted edits, not full file rewrites,
   for existing files
7. **Create parent dirs** -- use `mkdir -p` before writing to new paths
8. **Quote paths** -- always wrap paths containing spaces in double quotes
9. **Check before delete** -- verify `rm` targets before executing

## Error Recovery

If an edit goes wrong:

```bash
# Check git status
git status <file>

# Restore from git (discards ALL changes to the file)
git checkout -- <file>

# Restore from backup
cp <file>.bak <file>

# See what changed
git diff <file>
```
