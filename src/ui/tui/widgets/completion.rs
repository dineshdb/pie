use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Widget};

pub struct CompletionPopup<'a> {
    pub candidates: &'a [String],
    pub selected: usize,
}

impl CompletionPopup<'_> {
    /// Compute the popup area, positioned above the input area.
    pub fn popup_area(&self, input_area: Rect) -> Rect {
        #[allow(clippy::cast_possible_truncation)]
        let popup_height = self.candidates.len().min(6) as u16 + 2;
        #[allow(clippy::cast_possible_truncation)]
        let max_width = self
            .candidates
            .iter()
            .map(String::len)
            .max()
            .unwrap_or(10)
            .min(input_area.width as usize - 4) as u16
            + 4;

        Rect {
            x: input_area.x,
            y: input_area.y.saturating_sub(popup_height),
            width: max_width.min(input_area.width),
            height: popup_height,
        }
    }
}

impl Widget for CompletionPopup<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if self.candidates.is_empty() {
            return;
        }

        let items: Vec<Line> = self
            .candidates
            .iter()
            .enumerate()
            .map(|(i, cmd)| completion_line(cmd, i == self.selected))
            .collect();

        Paragraph::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::DarkGray))
                    .style(Style::default().bg(Color::Black)),
            )
            .render(area, buf);
    }
}

#[derive(Clone, Copy)]
pub enum Direction {
    Prev,
    Next,
}

pub struct CompletionState {
    candidates: Vec<String>,
    index: usize,
    all_commands: Vec<String>,
}

impl CompletionState {
    pub fn new(all_commands: Vec<String>) -> Self {
        Self {
            candidates: Vec::new(),
            index: 0,
            all_commands,
        }
    }

    pub fn is_active(&self) -> bool {
        !self.candidates.is_empty()
    }

    pub fn candidates(&self) -> &[String] {
        &self.candidates
    }

    pub fn index(&self) -> usize {
        self.index
    }

    pub fn move_selection(&mut self, direction: Direction) {
        if self.candidates.is_empty() {
            return;
        }
        self.index = match direction {
            Direction::Prev => {
                if self.index == 0 {
                    self.candidates.len() - 1
                } else {
                    self.index - 1
                }
            }
            Direction::Next => (self.index + 1) % self.candidates.len(),
        };
    }

    pub fn selected(&self) -> Option<&str> {
        self.candidates.get(self.index).map(String::as_str)
    }

    pub fn update(&mut self, line: &str) {
        if !line.starts_with('/') {
            self.reset();
            return;
        }

        let matches = find_completions(line, &self.all_commands);
        if matches.is_empty() || (matches.len() == 1 && matches.first() == Some(&line.to_string()))
        {
            self.reset();
            return;
        }

        let prev = self.candidates.get(self.index).cloned();
        self.candidates = matches;
        self.index = if let Some(prev_sel) = prev {
            self.candidates
                .iter()
                .position(|c| c == &prev_sel)
                .unwrap_or(0)
        } else {
            0
        };
    }

    pub fn reset(&mut self) {
        self.candidates.clear();
        self.index = 0;
    }

    pub fn find_hint(&self, line: &str) -> Option<String> {
        if !line.starts_with('/') || line.len() < 2 {
            return None;
        }
        self.all_commands
            .iter()
            .find(|cmd| cmd.starts_with(line) && cmd.as_str() != line)
            .map(|cmd| cmd[line.len()..].to_string())
    }
}

fn find_completions(prefix: &str, candidates: &[String]) -> Vec<String> {
    candidates
        .iter()
        .filter(|cmd| cmd.starts_with(prefix) && cmd.as_str() != prefix)
        .cloned()
        .collect()
}

fn completion_line(cmd: &str, is_selected: bool) -> Line<'static> {
    let style = if is_selected {
        Style::default()
            .fg(Color::Black)
            .bg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::White)
    };
    Line::from(Span::styled(format!(" {cmd} "), style))
}
