//! Non-interactive progress rendering. Single-shot runs are otherwise a
//! silent black box: [`spawn`] drains agent events into one stderr line per
//! phase — a tool call, or the model thinking/writing — redrawn in place
//! with a live elapsed-seconds counter. Only wired up when stderr is a real
//! terminal; piped stderr (scripts, `test.py`) stays untouched.

use crate::agent::AgentEvent;
use crate::agent::stream::truncate_for_log;
use crate::usage::RunUsage;
use std::io::IsTerminal;
use std::time::Instant;

pub(crate) fn spawn(label: String, event_rx: tokio::sync::mpsc::UnboundedReceiver<AgentEvent>) {
    if !std::io::stderr().is_terminal() {
        return;
    }
    tokio::spawn(run(label, event_rx));
}

/// Fallback when the terminal size cannot be probed.
const FALLBACK_WIDTH: usize = 80;

/// Room reserved for the " (123s)" suffix when clamping.
const SUFFIX_RESERVE: usize = 12;

/// The line shown for a phase: the activity, or the model silently working.
fn line_base(activity: Option<&str>) -> String {
    match activity {
        Some(a) => format!("· {a}"),
        None => "· thinking".to_string(),
    }
}

/// Clamp a line to the terminal width, reserving room for the time suffix
/// and marking the cut with an ellipsis. Char-boundary safe.
fn clamp_width(text: &str, width: usize) -> String {
    let max = width.saturating_sub(SUFFIX_RESERVE).max(20);
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut cut: String = text.chars().take(max - 1).collect();
    cut.push('…');
    cut
}

fn line_text(activity: Option<&str>, secs: u64, width: usize) -> String {
    format!("{} ({secs}s)", clamp_width(&line_base(activity), width))
}

fn draw_line(line_open: &mut bool, text: &str) {
    // stderr is unbuffered, so partial lines reach the terminal immediately.
    eprint!("\r\x1b[K{text}");
    *line_open = true;
}

fn close_line(line_open: &mut bool, text: &str) {
    if *line_open {
        eprint!("\r\x1b[K{text}");
        eprintln!();
        *line_open = false;
    }
}

async fn run(label: String, mut event_rx: tokio::sync::mpsc::UnboundedReceiver<AgentEvent>) {
    let width =
        termion::terminal_size().map_or(FALLBACK_WIDTH, |(w, _)| (w as usize).max(FALLBACK_WIDTH));
    let started = Instant::now();
    let mut phase_start = started;
    let mut activity: Option<String> = None;
    let mut line_open = false;
    let mut usage: Option<(RunUsage, Option<f64>)> = None;
    eprintln!("· {label}");

    let mut ticker = tokio::time::interval(std::time::Duration::from_secs(1));
    loop {
        tokio::select! {
            event = event_rx.recv() => {
                let Some(event) = event else { break };
                match event {
                    AgentEvent::ToolCall { display, .. } if !display.is_empty() => {
                        close_line(
                            &mut line_open,
                            &line_text(activity.as_deref(), phase_start.elapsed().as_secs(), width),
                        );
                        activity = Some(truncate_for_log(&display));
                        phase_start = Instant::now();
                        draw_line(
                            &mut line_open,
                            &line_text(activity.as_deref(), 0, width),
                        );
                    }
                    // Result half of a failed call: the failure gets its own
                    // line (the run itself continues — tool errors are
                    // handed back to the model, not fatal).
                    AgentEvent::ToolCall {
                        name, output, failed: true, ..
                    } => {
                        close_line(
                            &mut line_open,
                            &line_text(activity.as_deref(), phase_start.elapsed().as_secs(), width),
                        );
                        let reason = output.strip_prefix("Error: ").unwrap_or(&output);
                        eprintln!("! Tool {name} failed: {reason}");
                        activity = None;
                        phase_start = Instant::now();
                    }
                    AgentEvent::ToolCall { .. } => {
                        close_line(
                            &mut line_open,
                            &line_text(activity.as_deref(), phase_start.elapsed().as_secs(), width),
                        );
                        activity = None;
                        phase_start = Instant::now();
                    }
                    AgentEvent::Error(error) => {
                        close_line(
                            &mut line_open,
                            &line_text(activity.as_deref(), phase_start.elapsed().as_secs(), width),
                        );
                        activity = None;
                        phase_start = Instant::now();
                        eprintln!("! {error}");
                    }
                    AgentEvent::Usage {
                        usage: u,
                        cost_usd,
                    } => usage = Some((u, cost_usd)),
                    AgentEvent::Delta(_) if activity.is_none() => {
                        activity = Some("writing".to_string());
                        phase_start = Instant::now();
                    }
                    AgentEvent::Done(_) => {
                        close_line(
                            &mut line_open,
                            &line_text(activity.as_deref(), phase_start.elapsed().as_secs(), width),
                        );
                        let summary = usage
                            .filter(|(u, _)| u.requests > 0)
                            .map_or_else(String::new, |(u, cost)| {
                                format!(" · {}", u.summary(cost))
                            });
                        eprintln!("· done in {}s{summary}", started.elapsed().as_secs());
                        return;
                    }
                    _ => {}
                }
            }
            _ = ticker.tick() => {
                draw_line(
                    &mut line_open,
                    &line_text(activity.as_deref(), phase_start.elapsed().as_secs(), width),
                );
            }
        }
    }
    // Channel closed without Done (aborted run): don't leave a dangling line.
    close_line(
        &mut line_open,
        &line_text(activity.as_deref(), phase_start.elapsed().as_secs(), width),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_text_covers_activity_and_thinking() {
        assert_eq!(
            line_text(Some("Bash(git status)"), 3, 120),
            "· Bash(git status) (3s)"
        );
        assert_eq!(line_text(None, 12, 120), "· thinking (12s)");
    }

    #[test]
    fn clamp_keeps_short_lines_and_multibyte_safe_cuts_long_ones() {
        assert_eq!(clamp_width("short", 120), "short");

        let multibyte = "é".repeat(100);
        let cut = clamp_width(&multibyte, 40);
        assert!(cut.chars().count() <= 40 - SUFFIX_RESERVE, "{cut}");
        assert!(cut.ends_with('…'));
        assert!(cut.is_char_boundary(cut.len()));
    }
}
