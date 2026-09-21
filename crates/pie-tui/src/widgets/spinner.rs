use tuirealm::ratatui::buffer::Buffer;
use tuirealm::ratatui::layout::Rect;
use tuirealm::ratatui::style::Style;
use tuirealm::ratatui::widgets::Widget;

pub const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub struct Spinner {
    pub frame: usize,
    pub style: Style,
}

impl Spinner {
    pub fn new(frame: usize) -> Self {
        Self {
            frame,
            style: Style::default(),
        }
    }

    pub fn style(mut self, style: Style) -> Self {
        self.style = style;
        self
    }
}

impl Widget for Spinner {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let frame_idx = self.frame % SPINNER_FRAMES.len();
        if let Some(frame_str) = SPINNER_FRAMES.get(frame_idx) {
            buf.set_string(area.x, area.y, frame_str, self.style);
        }
    }
}
