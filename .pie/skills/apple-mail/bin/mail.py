#!/usr/bin/env python3
# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
import sys
import subprocess
import argparse
import json


def osa(script):
    try:
        result = subprocess.run(
            ["osascript", "-e", script], capture_output=True, text=True, check=True
        )
        return result.stdout.strip()
    except subprocess.CalledProcessError:
        return ""


def cmd_search(args):
    parser = argparse.ArgumentParser(prog="mail.py search")
    parser.add_argument("--from", dest="from_addr", default="")
    parser.add_argument("--subject", default="")
    parser.add_argument("--mailbox", default="INBOX")
    parser.add_argument("--account", default="dineshbhattrai0")
    parser.add_argument("--after", default="")
    parser.add_argument("--before", default="")
    parser.add_argument("--unread", action="store_true")
    parser.add_argument("--flagged", action="store_true")
    parser.add_argument("--limit", type=int, default=20)
    parser.add_argument("--md", "--markdown", dest="markdown", action="store_true")

    parsed, unknown = parser.parse_known_args(args)

    conditions = ""
    if parsed.unread:
        conditions += " and read status is false"
    if parsed.flagged:
        conditions += " and flagged status is true"

    script = f"""tell application "Mail"
        set msgs to messages of mailbox "{parsed.mailbox}" of account "{parsed.account}"{conditions}
        set n to count of msgs
        if n > {parsed.limit} then set n to {parsed.limit}
        set output to ""
        repeat with i from 1 to n
            set msg to item i of msgs
            try
                set msgId to message id of msg
                set msgSubject to subject of msg
                set msgSender to sender of msg
                set msgDate to date received of msg
                set skip to false
                if "{parsed.from_addr}" is not "" and msgSender does not contain "{parsed.from_addr}" then set skip to true
                if "{parsed.subject}" is not "" and msgSubject does not contain "{parsed.subject}" then set skip to true
                if not skip then
                    set output to output & msgId & tab & msgSubject & tab & msgSender & tab & (msgDate as string) & linefeed
                end if
            end try
        end repeat
        return output
    end tell"""

    raw = osa(script)
    if not parsed.markdown:
        print("id|sender|subject|date")

    if not raw:
        return

    for line in raw.split("\n"):
        if not line.strip():
            continue
        parts = line.split("\t")
        if len(parts) >= 4:
            msg_id = parts[0]
            subj = parts[1]
            sender = parts[2]
            date = parts[3]

            name = ""
            if "<" in sender:
                name = sender.split("<")[0].strip()
            else:
                name = sender

            if parsed.markdown:
                print(f"id: {msg_id}")
                print(f"sender: {name}")
                print(f"subject: {subj}")
                print(f"date: {date}")
                print("---")
            else:
                print(f"{msg_id}|{name}|{subj}|{date}")


def cmd_read(args):
    if not args:
        print("Usage: mail.py read <message_id>", file=sys.stderr)
        sys.exit(1)

    msg_id = args[0]

    script = f"""tell application "Mail"
        set allInboxes to {{inbox}}
        repeat with acct in every account
            try
                set end of allInboxes to inbox of acct
            end try
        end repeat
        
        repeat with theInbox in allInboxes
            set msgs to messages of theInbox
            repeat with msg in msgs
            try
                if message id of msg = "{msg_id}" then
                    set bodyText to content of msg
                    if bodyText is "" then
                        try
                            set bodyText to source of msg
                        end try
                    end if
                    return bodyText
                end if
            end try
        end repeat
        end repeat
        return "[not found]"
    end tell"""

    body = osa(script)
    if body == "[not found]":
        print(json.dumps({"error": f"Message not found: {msg_id}"}))
        sys.exit(1)
    print(body)


def cmd_send(args):
    parser = argparse.ArgumentParser(prog="mail.py send")
    parser.add_argument("--to", required=True)
    parser.add_argument("--subject", default="")
    parser.add_argument("--body", default="")
    parser.add_argument("--body-file", default="")

    parsed, unknown = parser.parse_known_args(args)

    body = parsed.body
    if parsed.body_file:
        with open(parsed.body_file, "r") as f:
            body = f.read()

    if not body and not sys.stdin.isatty():
        body = sys.stdin.read()

    if not body:
        body = "[no content]"

    escaped_body = body.replace("\\", "\\\\").replace('"', '\\"')
    escaped_subj = parsed.subject.replace("\\", "\\\\").replace('"', '\\"')

    script = f"""
        tell application "Mail"
            set newMsg to make new outgoing message with properties {{subject:"{escaped_subj}", content:"{escaped_body}"}}
            tell newMsg
                set visible to false
                make new to recipient at end of to recipients with properties {{address:"{parsed.to}"}}
                send
            end tell
        end tell
        return "sent"
    """

    try:
        subprocess.run(
            ["osascript", "-e", script], capture_output=True, text=True, check=True
        )
    except subprocess.CalledProcessError:
        # Retry with launching mail
        subprocess.run(["open", "-a", "Mail"], capture_output=True)
        import time

        time.sleep(3)
        subprocess.run(
            ["osascript", "-e", script], capture_output=True, text=True, check=True
        )


def cmd_accounts():
    script = """tell application "Mail"
        set accts to every account
        set output to ""
        repeat with acct in accts
            try
                set acctName to name of acct
                set acctType to account type of acct
                set emailAddrs to ""
                try
                    set emailAddrs to email addresses of acct
                end try
                set output to output & acctName & tab & (acctType as string) & tab & (emailAddrs as string) & linefeed
            end try
        end repeat
        return output
    end tell"""

    raw = osa(script)
    print("name|type|emails")
    if not raw:
        return

    for line in raw.split("\n"):
        if not line.strip():
            continue
        parts = line.split("\t")
        if len(parts) >= 3:
            print(f"{parts[0]}|{parts[1]}|{parts[2]}")


def cmd_mailboxes():
    script = """tell application "Mail"
        set output to ""
        repeat with acct in every account
            set acctName to name of acct
            
            -- Account specific inboxes / special mailboxes
            try
                set m to inbox of acct
                set output to output & acctName & tab & name of m & linefeed
            end try
            try
                set m to outbox of acct
                set output to output & acctName & tab & name of m & linefeed
            end try
            try
                set m to sent mailbox of acct
                set output to output & acctName & tab & name of m & linefeed
            end try
            try
                set m to junk mailbox of acct
                set output to output & acctName & tab & name of m & linefeed
            end try
            try
                set m to trash mailbox of acct
                set output to output & acctName & tab & name of m & linefeed
            end try
            try
                set m to drafts mailbox of acct
                set output to output & acctName & tab & name of m & linefeed
            end try

            -- Generic mailboxes
            repeat with mbox in every mailbox of acct
                set mboxName to name of mbox
                set output to output & acctName & tab & mboxName & linefeed
            end repeat
        end repeat
        return output
    end tell"""

    raw = osa(script)
    print("account|mailbox")
    if not raw:
        return

    for line in raw.split("\n"):
        if not line.strip():
            continue
        parts = line.split("\t")
        if len(parts) >= 2:
            print(f"{parts[0]}|{parts[1]}")


def cmd_perm():
    subprocess.run(
        [
            "open",
            "x-apple.systempreferences:com.apple.preference.security?Privacy_AllFiles",
        ],
        capture_output=True,
    )
    print("Open System Settings → Privacy & Security → Full Disk Access")


def cmd_help():
    print("""Usage: mail.py <command> [options]

Commands:
  search [flags]      Search emails (osascript)
  read <id>           Read email body
  send --to <addr>    Send email via Mail.app
  accounts            List Mail accounts
  mailboxes           List all mailboxes across accounts
  perm                Open Full Disk Access settings

Search flags: --from, --subject, --mailbox, --account, --after YYYY-MM-DD,
  --before YYYY-MM-DD, --unread, --flagged, --limit N, --md/--markdown""")


def main():
    if len(sys.argv) < 2:
        cmd_help()
        sys.exit(0)

    cmd = sys.argv[1]
    args = sys.argv[2:]

    if cmd == "search":
        cmd_search(args)
    elif cmd == "read":
        cmd_read(args)
    elif cmd == "send":
        cmd_send(args)
    elif cmd == "accounts":
        cmd_accounts()
    elif cmd == "mailboxes":
        cmd_mailboxes()
    elif cmd == "perm":
        cmd_perm()
    elif cmd in ("help", "--help", "-h"):
        cmd_help()
    else:
        print(f"Unknown command: {cmd}. Run 'mail.py help' for usage.", file=sys.stderr)
        sys.exit(1)


if __name__ == "__main__":
    main()
