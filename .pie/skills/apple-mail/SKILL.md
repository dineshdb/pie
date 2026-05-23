---
name: apple-mail
description: Search, read, and send emails via Apple Mail IPC. No TCC issues.
permissions:
  - unsandboxed:mail.py
---

# Apple Mail

Search, read, and send emails through Apple Mail via osascript IPC. Routes
through Mail.app's own process — no Full Disk Access needed.

Use --md for markdown rendering. 

## Usage

`mail.py <command> [options] --md`

### search

```
mail.py search --from "newsletter@" --limit 10
mail.py search --mailbox "INBOX" --unread
mail.py search --subject "weekly" --after 2026-01-01
```

Flags: `--from`, `--subject`, `--mailbox`, `--after`, `--before`,
`--unread`, `--flagged`, `--limit N`

Output: pipe-delimited `id|sender|subject|date`

### read

```
mail.py read <message_id>
```

Message ID is the Apple Mail `message id` string from search output.

### send

```
mail.py send --to "you@example.com" --subject "Hello" --body "..."
mail.py send --to "you@example.com" --subject "Hello" --body-file body.txt
echo "body" | mail.py send --to "you@example.com" --subject "Hello"
```

### accounts

```
mail.py accounts
```

Lists configured Mail accounts (name, type, emails).

### perm

```
mail.py perm
```

Opens System Settings → Privacy & Security → Full Disk Access.
(Not required for osascript commands, only for any SQLite-based tools.)

## Notes

- Search only scans the specified mailbox (default: INBOX)
- Message IDs are Apple Mail `message id` strings (not ROWIDs)
- `--from` and `--subject` are substring filters applied in AppleScript
- Mail.app must be running or launchable
- No TCC prompts — uses Apple Events IPC, not direct file access
