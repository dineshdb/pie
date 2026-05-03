use crate::agent::Agent;
use crate::skill::Skill;
use tuirealm::ratatui::layout::Rect;
use tuirealm::ratatui::style::{Color, Modifier, Style};
use tuirealm::ratatui::text::{Line, Span};
use tuirealm::ratatui::widgets::{Paragraph, Widget};

pub struct HelpOverlay<'a> {
    pub agents: &'a [Agent],
    pub skills: &'a [Skill],
    pub scroll_offset: u16,
}

impl Widget for HelpOverlay<'_> {
    fn render(self, area: Rect, buf: &mut tuirealm::ratatui::buffer::Buffer) {
        let mut lines: Vec<Line> = vec![
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
                Span::raw("       List agents and skills"),
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
                Span::styled("  Ctrl-T", Style::default().fg(Color::Green)),
                Span::raw("             Toggle plan list (Full / Compact)"),
            ]),
            Line::from(vec![
                Span::styled("  Esc", Style::default().fg(Color::Green)),
                Span::raw("               Close dialog / Cancel"),
            ]),
        ];

        if !self.agents.is_empty() {
            lines.push(Line::raw(""));
            lines.push(Line::from(Span::styled(
                "Agents",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )));
            lines.push(Line::raw(""));
            for agent in self.agents {
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("  /{}", agent.name),
                        Style::default().fg(Color::Green),
                    ),
                    Span::raw(format!("  {}", agent.description)),
                ]));
            }
        }

        if !self.skills.is_empty() {
            lines.push(Line::raw(""));
            lines.push(Line::from(Span::styled(
                "Skills",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )));
            lines.push(Line::raw(""));
            for skill in self.skills {
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("  /{}", skill.name),
                        Style::default().fg(Color::Green),
                    ),
                    Span::raw(format!("  {}", skill.description)),
                ]));
            }
        }

        Paragraph::new(lines)
            .scroll((self.scroll_offset, 0))
            .render(area, buf);
    }
}
