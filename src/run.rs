use std::time::Duration;

use ratatui::{
    crossterm::event::{self, poll, Event, KeyEventKind},
    DefaultTerminal,
};

use crate::app::App;
use crate::event::{handle_key, Action};
use crate::ui;

const POLL_INTERVAL: Duration = Duration::from_millis(16);

pub async fn run(mut terminal: DefaultTerminal, app: &mut App) {
    loop {
        app.poll_background().await;
        terminal
            .draw(|frame| ui::draw(frame, app))
            .expect("Failed to draw to terminal.");

        if !poll(Duration::ZERO).expect("Failed to poll.") {
            tokio::time::sleep(POLL_INTERVAL).await;
            continue;
        }

        let Event::Key(key) = event::read().expect("Failed to read event.") else {
            continue;
        };

        if key.kind != KeyEventKind::Press {
            continue;
        }

        match handle_key(app, key) {
            Action::None => {}
            Action::Search(query) => app.start_search(query),
            Action::Download => {
                app.start_selected_download();
                app.select_next();
            }
        }

        if app.should_quit {
            app.shutdown().await;
            break;
        }
    }
}
