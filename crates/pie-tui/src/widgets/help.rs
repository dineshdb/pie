use crate::theme::Theme;
use pie_core::agent::Agent;
use pie_core::registry::Skill;
use tuirealm::ratatui::layout::Rect;
use tuirealm::ratatui::style::{Modifier, Style};
use tuirealm::ratatui::text::{Line, Span};
use tuirealm::ratatui::widgets::{Paragraph, Widget};

pub struct HelpOverlay<'a> {
    pub agents: &'a [Agent],
    pub skills: &'a [Skill],
    pub scroll_offset: u16,
    pub theme: &'static Theme,
}

impl HelpOverlay<'_> {
    fn add_commands_section(lines: &mut Vec<Line>, theme: &Theme) {
        lines.push(Line::from(Span::styled(
            "Commands",
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::raw(""));
        let commands = [
            ("/help, /h", "          Show this help"),
            (
                "/model [id]",
                "        Model picker (or select id) — rides the next message",
            ),
            (
                "/mode [id]",
                "          Mode list (or select id) — rides the next message",
            ),
            (
                "/theme [name]",
                "       Color theme picker (or set dark/light)",
            ),
            (
                "/yolo",
                "             Toggle auto-approving permission asks",
            ),
            ("/skills, /ls", "       List agents and skills"),
            ("/clear", "             Clear conversation"),
            ("/exit, /quit", "        Exit"),
        ];
        for (cmd, desc) in commands {
            lines.push(Line::from(vec![
                Span::styled(format!("  {cmd}"), Style::default().fg(theme.success)),
                Span::raw(desc),
            ]));
        }
    }

    fn add_keys_section(lines: &mut Vec<Line>, theme: &Theme) {
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled(
            "Keys",
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::raw(""));
        let keys = [
            ("Enter", "              Send message"),
            ("Up/Down", "           Navigate history"),
            ("Page Up/Down", "     Scroll messages"),
            ("Ctrl-T", "             Toggle plan list (Full / Compact)"),
            ("Ctrl-K", "             Cycle mode (rides the next message)"),
            ("Esc", "               Close dialog / Cancel"),
        ];
        for (key, desc) in keys {
            lines.push(Line::from(vec![
                Span::styled(format!("  {key}"), Style::default().fg(theme.success)),
                Span::raw(desc),
            ]));
        }
    }

    fn add_agents_section(&self, lines: &mut Vec<Line>, theme: &Theme) {
        if self.agents.is_empty() {
            return;
        }
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled(
            "Agents",
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::raw(""));
        for agent in self.agents {
            lines.push(Line::from(vec![
                Span::styled(
                    format!("  /{}", agent.name),
                    Style::default().fg(theme.success),
                ),
                Span::raw(format!("  {}", agent.description)),
            ]));
        }
    }

    fn add_skills_section(&self, lines: &mut Vec<Line>, theme: &Theme) {
        if self.skills.is_empty() {
            return;
        }
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled(
            "Skills",
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::raw(""));
        for skill in self.skills {
            lines.push(Line::from(vec![
                Span::styled(
                    format!("  /{}", skill.name),
                    Style::default().fg(theme.success),
                ),
                Span::raw(format!("  {}", skill.description)),
            ]));
            for r in &skill.references {
                lines.push(Line::from(vec![
                    Span::raw(format!("    - {}: ", r.title)),
                    Span::styled(r.path.clone(), Style::default().fg(theme.link)),
                ]));
            }
        }
    }
}

impl Widget for HelpOverlay<'_> {
    fn render(self, area: Rect, buf: &mut tuirealm::ratatui::buffer::Buffer) {
        let mut lines: Vec<Line> = Vec::new();
        Self::add_commands_section(&mut lines, self.theme);
        Self::add_keys_section(&mut lines, self.theme);
        self.add_agents_section(&mut lines, self.theme);
        self.add_skills_section(&mut lines, self.theme);

        Paragraph::new(lines)
            .scroll((self.scroll_offset, 0))
            .render(area, buf);
    }
}
