use std::process::{Command, Stdio};

/// Send a macOS system notification. Fire-and-forget — spawned, never blocks.
pub fn notify(title: &str, body: &str) {
    let term = std::env::var("TERM_PROGRAM").unwrap_or_default();
    let bundle_id = match term.as_str() {
        "iTerm.app" => "com.googlecode.iterm2",
        "Apple_Terminal" => "com.apple.Terminal",
        "WarpTerminal" => "dev.warp.Warp-Stable",
        "vscode" => "com.microsoft.VSCode",
        _ => "",
    };

    if !bundle_id.is_empty() {
        let Ok(child) = Command::new("terminal-notifier")
            .arg("-title")
            .arg(title)
            .arg("-message")
            .arg(body)
            .arg("-activate")
            .arg(bundle_id)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        else {
            osascript_notify(title, body);
            return;
        };
        std::thread::spawn(move || {
            let _ = child.wait_with_output();
        });
        return;
    }

    osascript_notify(title, body);
}

/// Send a macOS system notification. Fire-and-forget — spawned, never blocks.
pub fn turn_complete(query: Option<&str>) {
    let title = "pie: Task Completed";
    let body = match query {
        Some(q) => truncate(q, 80),
        None => String::new(),
    };

    notify(title, &body);
}

fn osascript_notify(title: &str, body: &str) {
    let escaped_body = body.replace('\\', "\\\\").replace('"', "\\\"");
    let script = format!(
        "display notification \"{escaped_body}\" with title \"{title}\" sound name \"default\""
    );
    let Ok(child) = Command::new("osascript")
        .arg("-e")
        .arg(script)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return;
    };
    std::thread::spawn(move || {
        let _ = child.wait_with_output();
    });
}

fn truncate(s: &str, max_len: usize) -> String {
    let text = s.replace('\n', " ");
    if text.chars().count() <= max_len {
        text
    } else {
        let truncated: String = text.chars().take(max_len - 1).collect();
        format!("{truncated}…")
    }
}
