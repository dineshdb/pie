use tuirealm::ratatui::layout::Rect;
use tuirealm::ratatui::style::{Color, Modifier, Style};
use tuirealm::ratatui::text::{Line, Span};
use tuirealm::ratatui::widgets::{Paragraph, Widget};

pub struct HelpOverlay;

impl Widget for HelpOverlay {
    fn render(self, area: Rect, buf: &mut tuirealm::ratatui::buffer::Buffer) {
        let lines: Vec<Line> = vec![
            Line::from(Span::styled(
                "Commands",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::raw(""),
            Line::from(vec![
                Span::styled("  /help, /h", Style::default().fg(Color::Green)),
                Span::raw("          Show this help"),
            ]),
            Line::from(vec![
                Span::styled("  /model", Style::default().fg(Color::Green)),
                Span::raw("             List/switch models"),
            ]),
            Line::from(vec![
                Span::styled("  /skills, /ls", Style::default().fg(Color::Green)),
                Span::raw("       List agents"),
            ]),
            Line::from(vec![
                Span::styled("  /clear", Style::default().fg(Color::Green)),
                Span::raw("             Clear conversation"),
            ]),
            Line::from(vec![
                Span::styled("  /exit, /quit", Style::default().fg(Color::Green)),
                Span::raw("        Exit"),
            ]),
            Line::raw(""),
            Line::from(Span::styled(
                "Keys",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::raw(""),
            Line::from(vec![
                Span::styled("  Enter", Style::default().fg(Color::Green)),
                Span::raw("              Send message"),
            ]),
            Line::from(vec![
                Span::styled("  Up/Down", Style::default().fg(Color::Green)),
                Span::raw("           Navigate history"),
            ]),
            Line::from(vec![
                Span::styled("  Page Up/Down", Style::default().fg(Color::Green)),
                Span::raw("     Scroll messages"),
            ]),
            Line::from(vec![
                Span::styled("  Esc", Style::default().fg(Color::Green)),
                Span::raw("               Close dialog / Cancel"),
            ]),
        ];

        Paragraph::new(lines).render(area, buf);
    }
}
