use crate::registry::{CompletionItem, Registry};
use std::sync::Arc;
use tuirealm::ratatui::buffer::Buffer;
use tuirealm::ratatui::layout::Rect;
use tuirealm::ratatui::style::{Color, Modifier, Style};
use tuirealm::ratatui::text::{Line, Span};
use tuirealm::ratatui::widgets::{Block, Borders, Paragraph, Widget};

pub struct CompletionPopup<'a> {
    pub candidates: &'a [CompletionItem],
    pub selected: usize,
    /// Maximum height the popup can occupy (from input top to screen top).
    pub max_height: u16,
}

impl CompletionPopup<'_> {
    /// Number of visible item rows (excluding borders and overflow indicator).
    #[allow(clippy::cast_possible_truncation)]
    fn visible_rows(&self) -> u16 {
        let available = self.max_height.saturating_sub(2); // borders
        let count = self.candidates.len().min(u16::MAX as usize) as u16;
        let has_overflow = count > available;
        if has_overflow {
            available.saturating_sub(1) // reserve one row for overflow indicator
        } else {
            count.min(available)
        }
    }

    /// Compute the popup area, positioned above the input area.
    #[allow(clippy::cast_possible_truncation)]
    pub fn popup_area(&self, input_area: Rect) -> Rect {
        let visible = self.visible_rows();
        let overflow_row = u16::from(self.candidates.len() > visible as usize);
        let popup_height = (visible + overflow_row).min(self.max_height) + 2;
        let width = input_area.width;

        Rect {
            x: input_area.x,
            y: input_area.y.saturating_sub(popup_height),
            width,
            height: popup_height,
        }
    }
}

impl Widget for CompletionPopup<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if self.candidates.is_empty() {
            return;
        }

        let visible = self.visible_rows() as usize;
        let total = self.candidates.len();
        let scroll_offset = scroll_offset(self.selected, visible, total);

        let name_col_width = self
            .candidates
            .iter()
            .map(|c| c.label.len())
            .max()
            .unwrap_or(10)
            .min(area.width.saturating_sub(6) as usize)
            + 2;

        let end = (scroll_offset + visible).min(total);
        let visible_items = self.candidates.get(scroll_offset..end).unwrap_or_default();

        let mut items: Vec<Line> = visible_items
            .iter()
            .enumerate()
            .map(|(i, item)| {
                let is_selected = scroll_offset + i == self.selected;
                completion_line(item, is_selected, name_col_width, area.width)
            })
            .collect();

        // Overflow indicator
        let hidden_after = total.saturating_sub(scroll_offset + visible);
        if hidden_after > 0 {
            items.push(overflow_line(hidden_after, area.width));
        }

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

/// Compute scroll offset so that `selected` is always visible within `visible` rows.
fn scroll_offset(selected: usize, visible: usize, total: usize) -> usize {
    if total <= visible {
        return 0;
    }
    // Keep selected in view: if selected is past the bottom of the window, scroll down
    if selected >= visible {
        selected - visible + 1
    } else {
        0
    }
}

/// Find the last `/`-token in a line where `/` is at position 0 or after whitespace.
/// Returns `(start, end)` byte offsets.
pub fn slash_token_range(line: &str) -> Option<(usize, usize)> {
    let mut best = None;
    for (i, _) in line.char_indices().filter(|&(_, c)| c == '/') {
        if i > 0
            && !line
                .as_bytes()
                .get(i - 1)
                .is_some_and(u8::is_ascii_whitespace)
        {
            continue;
        }
        let end = line[i..]
            .find(|c: char| c.is_whitespace())
            .map_or(line.len(), |pos| i + pos);
        best = Some((i, end));
    }
    best
}

#[derive(Clone, Copy)]
pub enum Direction {
    Prev,
    Next,
}

pub struct CompletionState {
    candidates: Vec<CompletionItem>,
    index: usize,
    registry: Arc<Registry>,
}

impl CompletionState {
    pub fn new(registry: Arc<Registry>) -> Self {
        Self {
            candidates: Vec::new(),
            index: 0,
            registry,
        }
    }

    pub fn is_active(&self) -> bool {
        !self.candidates.is_empty()
    }

    pub fn candidates(&self) -> &[CompletionItem] {
        &self.candidates
    }

    pub fn index(&self) -> usize {
        self.index
    }

    pub fn move_selection(&mut self, direction: Direction) {
        if self.candidates.is_empty() {
            return;
        }
        let len = self.candidates.len();
        self.index = match direction {
            Direction::Prev => (self.index + len - 1) % len,
            Direction::Next => (self.index + 1) % len,
        };
    }

    pub fn selected(&self) -> Option<&str> {
        self.candidates.get(self.index).map(|c| c.label.as_str())
    }

    pub fn update(&mut self, line: &str) {
        let Some((start, end)) = slash_token_range(line) else {
            self.reset();
            return;
        };
        let token = &line[start..end];

        let matches = find_completions(token, &self.registry.completions);
        if matches.is_empty()
            || (matches.len() == 1 && matches.first().is_some_and(|c| c.label == token))
        {
            self.reset();
            return;
        }

        let prev = self.candidates.get(self.index).map(|c| c.label.clone());
        self.candidates = matches;
        self.index = if let Some(prev_sel) = prev {
            self.candidates
                .iter()
                .position(|c| c.label == prev_sel)
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
        let (start, end) = slash_token_range(line)?;
        let token = &line[start..end];
        if token.len() < 2 {
            return None;
        }
        self.registry
            .completions
            .iter()
            .find(|c| c.label.starts_with(token) && c.label != token)
            .map(|c| c.label[token.len()..].to_string())
    }
}

fn find_completions(prefix: &str, candidates: &[CompletionItem]) -> Vec<CompletionItem> {
    candidates
        .iter()
        .filter(|c| c.label.starts_with(prefix) && c.label != prefix)
        .cloned()
        .collect()
}

fn completion_line(
    item: &CompletionItem,
    is_selected: bool,
    name_width: usize,
    area_width: u16,
) -> Line<'static> {
    let name_style = if is_selected {
        Style::default()
            .fg(Color::Black)
            .bg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
            .fg(item.kind.color())
            .add_modifier(Modifier::BOLD)
    };

    let desc_style = if is_selected {
        Style::default().fg(Color::Black).bg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let name = format!(" {:<width$}", item.label, width = name_width);

    let inner_width = area_width.saturating_sub(2) as usize;
    let desc_max = inner_width.saturating_sub(name_width + 1);
    let desc = truncate_str(&item.description, desc_max);

    Line::from(vec![
        Span::styled(name, name_style),
        Span::styled(desc, desc_style),
    ])
}

/// Overflow indicator line shown at the bottom of the popup when items are clipped.
fn overflow_line(hidden: usize, area_width: u16) -> Line<'static> {
    let inner = area_width.saturating_sub(2) as usize;
    let text = format!("  \u{25BE} {hidden} more");
    let truncated = truncate_str(&text, inner);
    Line::from(Span::styled(
        truncated,
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::ITALIC),
    ))
}

/// Truncate a string to fit within `max_chars`, appending "..." if truncated.
fn truncate_str(s: &str, max_chars: usize) -> String {
    if max_chars <= 3 || s.len() <= max_chars {
        return s.to_string();
    }
    let end = s
        .char_indices()
        .take_while(|(i, _)| *i < max_chars - 3)
        .last()
        .map_or(max_chars.min(s.len()), |(i, c)| i + c.len_utf8());
    format!("{}...", &s[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::CompletionKind;

    fn test_registry(items: Vec<(&str, &str, CompletionKind)>) -> Arc<Registry> {
        Arc::new(Registry {
            agents: Vec::new(),
            skills: Vec::new(),
            completions: items
                .into_iter()
                .map(|(label, desc, kind)| CompletionItem {
                    label: label.to_string(),
                    description: desc.to_string(),
                    kind,
                })
                .collect(),
        })
    }

    #[test]
    fn slash_token_at_start() {
        assert_eq!(slash_token_range("/hello world"), Some((0, 6)));
    }

    #[test]
    fn slash_token_after_space() {
        assert_eq!(slash_token_range("use /rev now"), Some((4, 8)));
    }

    #[test]
    fn slash_token_no_slash() {
        assert_eq!(slash_token_range("hello world"), None);
    }

    #[test]
    fn slash_token_embedded_in_word() {
        assert_eq!(slash_token_range("foo/bar"), None);
    }

    #[test]
    fn slash_token_returns_last_when_multiple() {
        assert_eq!(slash_token_range("/first /second end"), Some((7, 14)));
    }

    #[test]
    fn slash_token_at_end_of_line() {
        assert_eq!(slash_token_range("use /review"), Some((4, 11)));
    }

    #[test]
    fn slash_token_only_slash() {
        assert_eq!(slash_token_range("/"), Some((0, 1)));
    }

    #[test]
    fn slash_token_empty_string() {
        assert_eq!(slash_token_range(""), None);
    }

    #[test]
    fn slash_token_path_is_full_token() {
        // No whitespace in "/usr/bin/bash" so it's one token
        assert_eq!(slash_token_range("/usr/bin/bash"), Some((0, 13)));
    }

    #[test]
    fn slash_token_second_slash_embedded() {
        // /bin is embedded in path, not at word boundary; whole /usr/bin is one token
        assert_eq!(slash_token_range("use /usr/bin"), Some((4, 12)));
    }

    #[test]
    fn update_triggers_at_start() {
        let reg = test_registry(vec![
            ("/review", "Review code", CompletionKind::Skill),
            ("/refactor", "Refactor", CompletionKind::Skill),
        ]);
        let mut state = CompletionState::new(reg);
        state.update("/re");
        assert!(state.is_active());
        assert_eq!(state.candidates().len(), 2);
    }

    #[test]
    fn update_triggers_mid_line() {
        let reg = test_registry(vec![("/debug", "Debug", CompletionKind::Agent)]);
        let mut state = CompletionState::new(reg);
        state.update("use /deb");
        assert!(state.is_active());
        assert_eq!(state.candidates().len(), 1);
        assert_eq!(state.selected(), Some("/debug"));
    }

    #[test]
    fn update_no_match_resets() {
        let reg = test_registry(vec![("/review", "Review", CompletionKind::Skill)]);
        let mut state = CompletionState::new(reg);
        state.update("/zzz");
        assert!(!state.is_active());
    }

    #[test]
    fn update_exact_match_resets() {
        let reg = test_registry(vec![("/review", "Review", CompletionKind::Skill)]);
        let mut state = CompletionState::new(reg);
        state.update("/review");
        assert!(!state.is_active(), "exact match should not show popup");
    }

    #[test]
    fn update_no_slash_resets() {
        let reg = test_registry(vec![("/review", "Review", CompletionKind::Skill)]);
        let mut state = CompletionState::new(reg);
        state.update("/rev");
        assert!(state.is_active());
        state.update("no slash here");
        assert!(!state.is_active());
    }

    #[test]
    fn update_preserves_selection() {
        let reg = test_registry(vec![
            ("/review", "Review", CompletionKind::Skill),
            ("/refactor", "Refactor", CompletionKind::Skill),
        ]);
        let mut state = CompletionState::new(reg);
        state.update("/re");
        assert_eq!(state.candidates().len(), 2);
        state.move_selection(Direction::Next);
        assert_eq!(state.selected(), Some("/refactor"));
        // Narrow to /rev — only /review matches, selection falls back to 0
        state.update("/rev");
        assert_eq!(state.candidates().len(), 1);
        assert_eq!(state.selected(), Some("/review"));
    }

    #[test]
    fn find_hint_at_start() {
        let reg = test_registry(vec![("/review", "Review", CompletionKind::Skill)]);
        let state = CompletionState::new(reg);
        assert_eq!(state.find_hint("/rev"), Some("iew".to_string()));
    }

    #[test]
    fn find_hint_mid_line() {
        let reg = test_registry(vec![("/debug", "Debug", CompletionKind::Agent)]);
        let state = CompletionState::new(reg);
        assert_eq!(state.find_hint("use /deb"), Some("ug".to_string()));
    }

    #[test]
    fn find_hint_exact_match_returns_none() {
        let reg = test_registry(vec![("/review", "Review", CompletionKind::Skill)]);
        let state = CompletionState::new(reg);
        assert_eq!(state.find_hint("/review"), None);
    }

    #[test]
    fn find_hint_no_match_returns_none() {
        let reg = test_registry(vec![("/review", "Review", CompletionKind::Skill)]);
        let state = CompletionState::new(reg);
        assert_eq!(state.find_hint("/zzz"), None);
    }

    #[test]
    fn find_hint_slash_only_returns_none() {
        let reg = test_registry(vec![("/review", "Review", CompletionKind::Skill)]);
        let state = CompletionState::new(reg);
        assert_eq!(state.find_hint("/"), None);
    }

    #[test]
    fn find_hint_no_slash_returns_none() {
        let reg = test_registry(vec![("/review", "Review", CompletionKind::Skill)]);
        let state = CompletionState::new(reg);
        assert_eq!(state.find_hint("no slash"), None);
    }

    // ── apply_completion logic ─────────────────────────────────

    #[test]
    fn apply_completion_preserves_prefix_and_suffix() {
        let line = "use /rev now";
        let (start, end) = slash_token_range(line).unwrap();
        let completion = "/review";
        let result = format!("{}{}{}", &line[..start], completion, &line[end..]);
        assert_eq!(result, "use /review now");
    }

    #[test]
    fn apply_completion_at_start_no_suffix() {
        let line = "/rev";
        let (start, end) = slash_token_range(line).unwrap();
        let completion = "/review";
        let result = format!("{}{}{}", &line[..start], completion, &line[end..]);
        assert_eq!(result, "/review");
    }

    #[test]
    fn apply_completion_mid_line_no_suffix() {
        let line = "use /deb";
        let (start, end) = slash_token_range(line).unwrap();
        let completion = "/debug";
        let result = format!("{}{}{}", &line[..start], completion, &line[end..]);
        assert_eq!(result, "use /debug");
    }

    // ── CompletionKind::color ──────────────────────────────────

    #[test]
    fn completion_kind_colors() {
        assert_eq!(CompletionKind::Builtin.color(), Color::Yellow);
        assert_eq!(CompletionKind::Skill.color(), Color::Cyan);
        assert_eq!(CompletionKind::Agent.color(), Color::Green);
    }
}
