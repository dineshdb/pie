use std::io::{self, IsTerminal, Write};
use std::time::Instant;

use termimad::MadSkin;
use termimad::crossterm::{
    cursor::{MoveUp, Show as CursorShow},
    execute, queue,
    style::Print,
    terminal::{Clear, ClearType},
};

const RENDER_THROTTLE: std::time::Duration = std::time::Duration::from_millis(60);

pub struct MarkdownRenderer {
    skin: MadSkin,
    buffer: String,
    lines_rendered: usize,
    last_render: Instant,
    is_terminal: bool,
}

impl MarkdownRenderer {
    pub fn new() -> Self {
        let is_terminal = io::stdout().is_terminal();
        Self {
            skin: MadSkin::default(),
            buffer: String::new(),
            lines_rendered: 0,
            last_render: Instant::now()
                .checked_sub(RENDER_THROTTLE)
                .unwrap_or_else(Instant::now),
            is_terminal,
        }
    }

    /// Append a text delta from the stream and re-render if enough time has passed.
    pub fn push_delta(&mut self, delta: &str) {
        if delta.is_empty() {
            return;
        }
        self.buffer.push_str(delta);
        if self.last_render.elapsed() >= RENDER_THROTTLE {
            self.render();
        }
    }

    /// Re-render the current buffer, replacing previously rendered lines.
    fn render(&mut self) {
        if self.is_terminal {
            self.render_terminal();
        } else {
            // Non-terminal: just accumulate, no incremental rendering
        }
        self.last_render = Instant::now();
    }

    fn render_terminal(&mut self) {
        let stdout = io::stdout();
        let mut handle = stdout.lock();

        // Move cursor up and clear previous rendered lines
        if self.lines_rendered > 0 {
            queue!(handle, MoveUp(self.lines_rendered as u16)).ok();
            for _ in 0..self.lines_rendered {
                queue!(handle, Clear(ClearType::CurrentLine), Print("\n")).ok();
            }
            queue!(handle, MoveUp(self.lines_rendered as u16)).ok();
        }

        // Render markdown via termimad
        let rendered = self.skin.term_text(&self.buffer);
        let rendered_str = rendered.to_string();
        self.lines_rendered = rendered_str.lines().count();

        // Print each line (termimad's output includes its own styling via crossterm)
        for line in rendered_str.lines() {
            queue!(handle, Print(line), Print("\r\n")).ok();
        }

        handle.flush().ok();
    }

    /// Final render - flush remaining buffered content and return the full text.
    /// For terminals, does one final redraw. For non-terminals, prints everything.
    pub fn finish(mut self) -> String {
        if self.is_terminal {
            // Final render to catch any throttled content
            self.render_terminal();
            // Show cursor (just in case)
            execute!(io::stdout(), CursorShow).ok();
        } else {
            // Non-terminal: print all accumulated text at once
            if !self.buffer.is_empty() {
                let rendered = self.skin.term_text(&self.buffer);
                println!("{rendered}");
            }
        }
        self.buffer
    }
}

impl Default for MarkdownRenderer {
    fn default() -> Self {
        Self::new()
    }
}
