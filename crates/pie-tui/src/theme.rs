//! Color themes for the TUI.
//!
//! The frontend used to hardcode bright ANSI palette colors (White /
//! Gray / Yellow / Cyan) tuned for a dark terminal background — on a
//! light-background terminal that text is invisible. Every surface now
//! asks its [`Theme`] for a *role* (`text`, `accent`, `selection_bg`,
//! …) instead of a color, and two palettes ship: `dark` (the original
//! dark-terminal palette) and `light` (dark text on light surfaces).
//!
//! The current theme is UI-chrome state, swapped only by `/theme` on
//! the UI thread and read when a frame's widgets are constructed.
//! Widget logic never reads the global — it receives `&'static Theme`
//! explicitly, so tests render a named theme without touching it.
//! The choice persists under the pie home directory, best-effort like
//! the input history.

use pie_core::config::pie_home;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU8, Ordering};
use tuirealm::ratatui::style::Color;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    pub name: &'static str,
    /// Primary readable text: user messages, input, list entries.
    pub text: Color,
    /// Long-form body text: assistant responses.
    pub text_secondary: Color,
    /// Decorative text: prefixes, hints, dim borders, tool output.
    pub text_dim: Color,
    /// Titles, dialog borders, the mode tag, the streaming spinner.
    pub accent: Color,
    /// The input prompt, confirmations, agent completions.
    pub success: Color,
    /// System notices, builtin completions.
    pub warning: Color,
    /// URLs and skill reference paths.
    pub link: Color,
    /// Tool call lines.
    pub tool: Color,
    /// Highlighted row text / background (picker selection).
    pub selection_fg: Color,
    pub selection_bg: Color,
    /// Popup backgrounds (completion list).
    pub surface: Color,
    /// Code text / background in markdown.
    pub code_fg: Color,
    pub code_bg: Color,
}

impl Theme {
    /// Completion-kind → role, mapped here in the frontend: pie-core
    /// stays frontend-agnostic.
    pub fn completion_kind_color(&self, kind: pie_core::registry::CompletionKind) -> Color {
        use pie_core::registry::CompletionKind;
        match kind {
            CompletionKind::Builtin => self.warning,
            CompletionKind::Skill => self.accent,
            CompletionKind::Agent => self.success,
        }
    }
}

/// The dark-background palette.
///
/// Both palettes use truecolor RGB on purpose: palette-indexed ANSI
/// colors inherit whatever the terminal's theme defines, and a terminal
/// whose 16-color palette lacks contrast makes every indexed color
/// unreadable — and every theme switch invisible. RGB ignores the
/// terminal palette; a terminal without truecolor support falls back to
/// its default foreground, which is still readable.
pub static DARK: Theme = Theme {
    name: "dark",
    text: Color::Rgb(230, 233, 236),
    text_secondary: Color::Rgb(178, 186, 194),
    text_dim: Color::Rgb(120, 129, 138),
    accent: Color::Rgb(97, 214, 242),
    success: Color::Rgb(124, 202, 141),
    warning: Color::Rgb(230, 209, 116),
    link: Color::Rgb(129, 162, 247),
    tool: Color::Rgb(214, 143, 240),
    selection_fg: Color::Rgb(13, 17, 23),
    selection_bg: Color::Rgb(97, 214, 242),
    surface: Color::Rgb(22, 27, 34),
    code_fg: Color::Rgb(124, 202, 141),
    code_bg: Color::Rgb(13, 17, 23),
};

/// The light-background palette — near-black text on light surfaces.
pub static LIGHT: Theme = Theme {
    name: "light",
    text: Color::Rgb(24, 26, 27),
    text_secondary: Color::Rgb(66, 70, 73),
    text_dim: Color::Rgb(128, 134, 139),
    accent: Color::Rgb(9, 105, 218),
    success: Color::Rgb(26, 110, 52),
    warning: Color::Rgb(140, 86, 8),
    link: Color::Rgb(9, 105, 218),
    tool: Color::Rgb(118, 63, 177),
    selection_fg: Color::Rgb(255, 255, 255),
    selection_bg: Color::Rgb(9, 105, 218),
    surface: Color::Rgb(242, 243, 244),
    code_fg: Color::Rgb(26, 110, 52),
    code_bg: Color::Rgb(236, 238, 240),
};

pub static THEMES: &[&Theme] = &[&DARK, &LIGHT];

static CURRENT: AtomicU8 = AtomicU8::new(0);

/// The theme new frames render with (`dark` until `/theme` switches it).
pub fn current() -> &'static Theme {
    let idx = CURRENT.load(Ordering::Relaxed);
    THEMES.get(usize::from(idx)).copied().unwrap_or(&DARK)
}

/// Swap the current theme; unknown names keep the current one.
pub fn set_by_name(name: &str) -> Option<&'static Theme> {
    let theme = by_name(name)?;
    set(theme);
    Some(theme)
}

pub fn set(theme: &'static Theme) {
    if let Some(idx) = THEMES.iter().position(|t| t == &theme)
        && let Ok(idx) = u8::try_from(idx)
    {
        CURRENT.store(idx, Ordering::Relaxed);
    }
}

pub fn by_name(name: &str) -> Option<&'static Theme> {
    THEMES.iter().copied().find(|t| t.name == name)
}

/// Where the current theme sits in [`THEMES`] — the picker's starting row.
pub fn current_index() -> usize {
    THEMES
        .iter()
        .position(|t| t.name == current().name)
        .unwrap_or(0)
}

/// The theme saved from the last session, or the default.
pub fn load_saved() -> &'static Theme {
    std::fs::read_to_string(saved_path())
        .ok()
        .and_then(|s| by_name(s.trim()))
        .unwrap_or(&DARK)
}

/// Persist the current theme, best-effort — cosmetic state, never an
/// error surfaced to the user.
pub fn save_current() {
    let _ = std::fs::create_dir_all(pie_home());
    let _ = std::fs::write(saved_path(), current().name);
}

fn saved_path() -> PathBuf {
    pie_home().join("theme")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_theme_resolves_by_its_name() {
        for theme in THEMES {
            assert_eq!(by_name(theme.name), Some(*theme));
        }
        assert_eq!(by_name("mauve"), None, "unknown names must not resolve");
    }

    #[test]
    fn default_theme_is_dark() {
        assert_eq!(current().name, "dark");
    }

    /// The bug this module exists for: the light theme must not reuse
    /// the light-on-light palette — its text differs from dark's.
    #[test]
    fn light_theme_text_is_not_the_dark_palette() {
        assert_ne!(LIGHT.text, DARK.text);
        assert_ne!(LIGHT.text, Color::White, "white text is invisible on white");
        assert_ne!(LIGHT.text_secondary, DARK.text_secondary);
    }

    /// Palette-indexed colors inherit the terminal's 16-color theme —
    /// the exact dependency that made text unreadable on some
    /// terminals. Every role must be truecolor.
    #[test]
    fn themes_are_truecolor_not_terminal_palette_dependent() {
        for theme in THEMES {
            let debug = format!("{theme:?}");
            assert!(debug.contains("Rgb("), "{theme:?} must use truecolor");
            assert!(
                !debug.contains("Indexed"),
                "{theme:?} must not use palette indexes"
            );
        }
    }

    #[test]
    fn current_index_tracks_the_current_theme() {
        set(&DARK);
        assert_eq!(current_index(), 0);
        set(&LIGHT);
        assert_eq!(current_index(), 1);
        set(&DARK);
    }

    #[test]
    fn set_by_name_round_trips_and_rejects_unknown() {
        set(&DARK);
        assert!(set_by_name("light").is_some());
        assert_eq!(current().name, "light");
        assert_eq!(set_by_name("mauve"), None);
        assert_eq!(current().name, "light", "a rejected name must not switch");
        set(&DARK);
        assert_eq!(current().name, "dark");
    }
}
