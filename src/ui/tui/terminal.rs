use crate::providers::Model;
use crate::session::Session;
use crate::ui::tui::event::{AppEvent, HandleResult};
use crate::ui::tui::model::AppModel;
use anyhow::Result;
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event as CrosstermEvent, MouseEventKind,
};
use crossterm::execute;
use ratatui::crossterm;
use std::io::stdout;
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::mpsc;

const FRAME_DURATION: Duration = Duration::from_millis(50);

pub async fn run_tui(model: Model, session: Session, sandbox_settings: PathBuf) -> Result<()> {
    let mut terminal = ratatui::init();
    execute!(stdout(), EnableMouseCapture)?;
    terminal.clear()?;

    let mut app = AppModel::new(model, &session, sandbox_settings);

    let (tx, mut rx) = mpsc::unbounded_channel::<AppEvent>();

    terminal.draw(|f| app.render(f))?;

    loop {
        let crossterm_event = tokio::task::spawn_blocking(|| event::poll(FRAME_DURATION))
            .await
            .unwrap_or(Ok(false))?;

        if crossterm_event && let Some(app_event) = convert_event(event::read()?) {
            match app_event {
                AppEvent::Key(key) => {
                    let result = app.handle_key(key);
                    if result == HandleResult::Submit {
                        app.submit_input(&tx);
                    }
                    if result == HandleResult::Quit || app.should_quit {
                        break;
                    }
                }
                other => app.handle_event(other),
            }
        }

        while let Ok(ev) = rx.try_recv() {
            app.handle_event(ev);
        }

        terminal.draw(|f| app.render(f))?;
    }

    execute!(stdout(), DisableMouseCapture)?;
    ratatui::restore();
    Ok(())
}

#[allow(clippy::needless_pass_by_value)]
fn convert_event(e: CrosstermEvent) -> Option<AppEvent> {
    match e {
        CrosstermEvent::Key(key) => Some(AppEvent::Key(key)),
        CrosstermEvent::Resize(_, _) => Some(AppEvent::Resize),
        CrosstermEvent::Mouse(mouse) => match mouse.kind {
            MouseEventKind::ScrollUp => Some(AppEvent::ScrollUp),
            MouseEventKind::ScrollDown => Some(AppEvent::ScrollDown),
            _ => None,
        },
        _ => None,
    }
}
