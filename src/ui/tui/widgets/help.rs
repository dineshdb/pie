use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph, Widget};

pub struct HelpOverlay;

impl Widget for HelpOverlay {
    fn render(self, area: Rect, buf: &mut ratatui::buffer::Buffer) {
        Clear.render(area, buf);

        let lines: Vec<Line> = vec![
            Line::from(Span::styled(
                "  pie help",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::raw(""),
            Line::from(vec![
                Span::styled("  /help, /h", Style::default().fg(Color::Green)),
                Span::raw("          Show this help          "),
                Span::styled("/skills, /ls", Style::default().fg(Color::Green)),
                Span::raw("  List agents"),
            ]),
            Line::from(vec![
                Span::styled("  /clear", Style::default().fg(Color::Green)),
                Span::raw("             Clear conversation   "),
                Span::styled("/exit, /quit, /q", Style::default().fg(Color::Green)),
                Span::raw("  Exit"),
            ]),
            Line::raw(""),
            Line::from(Span::styled(
                "  Keys",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::raw(""),
            Line::from(vec![
                Span::styled("  Enter", Style::default().fg(Color::Green)),
                Span::raw("              Send message          "),
                Span::styled("Ctrl+Enter", Style::default().fg(Color::Green)),
                Span::raw("  New line"),
            ]),
            Line::from(vec![
                Span::styled("  Up/Down", Style::default().fg(Color::Green)),
                Span::raw("           Navigate history        "),
                Span::styled("Esc", Style::default().fg(Color::Green)),
                Span::raw("  Cancel streaming"),
            ]),
            Line::from(vec![
                Span::styled("  Page Up/Down", Style::default().fg(Color::Green)),
                Span::raw("     Scroll messages          "),
                Span::styled("Ctrl+c", Style::default().fg(Color::Green)),
                Span::raw("  Quit"),
            ]),
        ];

        Paragraph::new(lines)
            .style(Style::default().bg(Color::Black))
            .render(area, buf);
    }
}
