mod support;

use libgen_tui::{
    app::{
        App, AppConfig, DownloadStatus, Downloads, Focus, ResultsState, MAX_CONCURRENT_DOWNLOADS,
    },
    event::{handle_key, Action},
    libgen::{search, Book},
};
use md5::{Digest, Md5};
use ratatui::{
    backend::TestBackend,
    crossterm::event::{KeyCode, KeyEvent, KeyModifiers},
    Terminal,
};
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use support::{Reply, Server};

fn app(dir: &tempfile::TempDir) -> App {
    App::new(AppConfig {
        additional_mirrors: vec![],
        download_directory: dir.path().to_str().unwrap().into(),
        max_results: 25,
    })
}
fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}
fn html(title: &str) -> String {
    format!("<table id='tablelibgen'><tbody><tr><td><a>{title}</a></td><td>A</td><td>P</td><td>2020</td><td>English</td><td>1</td><td>1 KB</td><td>pdf</td><td><a href='ads.php?md5=0123456789abcdef0123456789abcdef'>get</a></td></tr></tbody></table>")
}
async fn settle(app: &mut App) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while app.results_state == ResultsState::Searching {
            app.poll_background().await;
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
}

#[test]
fn annotations_do_not_remove_identical_visible_text() {
    let books = search::parse_books(&html(
        "Rust <span>language</span><i>Rust <b>language</b></i>",
    ));
    assert_eq!(books[0].title, "Rust language");
}

#[test]
fn invalid_search_clears_hidden_results_and_checks_unicode_characters() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app(&dir);
    app.set_results(search::parse_books(&html("Old book")));
    app.focus = Focus::SearchBar;
    app.search_bar.insert_str("書");
    assert_eq!(handle_key(&mut app, key(KeyCode::Enter)), Action::None);
    assert_eq!(app.results_state, ResultsState::QueryTooShort);
    assert!(app.search_results.is_empty());
    assert!(app.selected_book().is_none());
    handle_key(&mut app, key(KeyCode::Tab));
    assert_eq!(app.focus, Focus::SearchBar);
    app.focus = Focus::Table;
    assert_eq!(handle_key(&mut app, key(KeyCode::Char(' '))), Action::None);
    app.focus = Focus::SearchBar;
    app.search_bar.insert_str("籍");
    assert!(matches!(
        handle_key(&mut app, key(KeyCode::Enter)),
        Action::Search(_)
    ));
}

#[test]
fn duplicate_jobs_are_rejected_but_failed_jobs_can_retry() {
    let jobs = Downloads::new();
    assert!(jobs.start("book", "id"));
    assert!(!jobs.start("book", "id"));
    jobs.fail("id", "network");
    assert!(jobs.start("book", "id"));
    assert!(jobs.snapshot()[0].error.is_none());
    jobs.complete("id");
    assert!(!jobs.start("book", "id"));
}

#[tokio::test]
async fn new_search_cancels_old_result_and_quit_remains_responsive() {
    let server = Server::new(|path| {
        if path.contains("old") {
            std::thread::sleep(Duration::from_millis(180));
            Reply::ok(html("Old"))
        } else {
            Reply::ok(html("New"))
        }
    });
    let dir = tempfile::tempdir().unwrap();
    let mut app = app(&dir);
    app.active_mirror = Some(server.url.clone());
    app.mirrors = vec![server.url.clone()];
    app.start_search("old".into());
    tokio::time::sleep(Duration::from_millis(20)).await;
    app.start_search("new".into());
    settle(&mut app).await;
    assert_eq!(
        app.selected_book().map(|b| b.title.as_str()),
        Some("New"),
        "{:?}",
        app.results_state
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    app.poll_background().await;
    assert_eq!(
        app.selected_book().map(|b| b.title.as_str()),
        Some("New"),
        "{:?}",
        app.results_state
    );
    app.start_search("old".into());
    handle_key(
        &mut app,
        KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
    );
    assert!(app.should_quit);
    tokio::time::timeout(Duration::from_millis(100), app.shutdown())
        .await
        .unwrap();
}

#[tokio::test]
async fn invalid_query_cancels_inflight_search_and_failure_clears_selection() {
    let server = Server::new(|_| {
        std::thread::sleep(Duration::from_millis(60));
        Reply::ok(html("Old"))
    });
    let dir = tempfile::tempdir().unwrap();
    let mut app = app(&dir);
    app.active_mirror = Some(server.url.clone());
    app.start_search("old".into());
    app.focus = Focus::SearchBar;
    handle_key(&mut app, key(KeyCode::Enter));
    tokio::time::sleep(Duration::from_millis(100)).await;
    app.poll_background().await;
    assert_eq!(app.results_state, ResultsState::QueryTooShort);
    assert!(app.selected_book().is_none());
    let bad = Server::new(|_| Reply::ok("not a search page"));
    app.set_results(search::parse_books(&html("Old")));
    app.active_mirror = Some(bad.url.clone());
    app.mirrors = vec![bad.url.clone()];
    app.start_search("query".into());
    settle(&mut app).await;
    assert!(matches!(app.results_state, ResultsState::Failed(_)));
    assert!(app.selected_book().is_none());
}

#[tokio::test]
async fn application_bounds_transfers_and_suppresses_duplicate_selection() {
    let bodies: Vec<_> = (0..8).map(|i| format!("book {i}")).collect();
    let books: Vec<_> = bodies
        .iter()
        .map(|body| Book {
            title: "Same title".into(),
            extension: "pdf".into(),
            md5: format!("{:x}", Md5::digest(body.as_bytes())),
            ..Default::default()
        })
        .collect();
    let active = Arc::new(AtomicUsize::new(0));
    let maximum = Arc::new(AtomicUsize::new(0));
    let count = Arc::new(AtomicUsize::new(0));
    let (a, m, c) = (active.clone(), maximum.clone(), count.clone());
    let catalog = books.clone();
    let server = Server::new(move |path| {
        if path.starts_with("/ads.php") {
            let index = catalog.iter().position(|b| path.contains(&b.md5)).unwrap();
            Reply::ok(format!("<a href='/get.php?id={index}'>get</a>"))
        } else {
            c.fetch_add(1, Ordering::SeqCst);
            let now = a.fetch_add(1, Ordering::SeqCst) + 1;
            m.fetch_max(now, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(80));
            a.fetch_sub(1, Ordering::SeqCst);
            let index: usize = path.split('=').nth(1).unwrap().parse().unwrap();
            Reply::ok(bodies[index].clone())
        }
    });
    let dir = tempfile::tempdir().unwrap();
    let mut app = app(&dir);
    app.active_mirror = Some(server.url.clone());
    app.mirrors = vec![server.url.clone()];
    app.set_results(books);
    for index in 0..8 {
        app.table_state.select(Some(index));
        app.start_selected_download();
        app.start_selected_download();
    }
    tokio::time::timeout(Duration::from_secs(5), async {
        while app
            .downloads
            .snapshot()
            .iter()
            .any(|d| d.status == DownloadStatus::Pending)
        {
            app.poll_background().await;
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(
        app.downloads
            .snapshot()
            .iter()
            .all(|d| d.status == DownloadStatus::Completed),
        "{:?}",
        app.downloads.snapshot()
    );
    assert_eq!(count.load(Ordering::SeqCst), 8);
    assert!(maximum.load(Ordering::SeqCst) <= MAX_CONCURRENT_DOWNLOADS);
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 8);
    app.shutdown().await;
}

#[test]
fn upgraded_widgets_render_results_popup_and_empty_small_screens() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app(&dir);
    for (width, height) in [(100, 30), (20, 5), (1, 1)] {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|f| libgen_tui::ui::draw(f, &mut app))
            .unwrap();
        app.set_results(search::parse_books(&html("A book")));
        app.show_popup = true;
        app.focus = Focus::PopupYes;
        terminal
            .draw(|f| libgen_tui::ui::draw(f, &mut app))
            .unwrap();
        app.show_popup = false;
    }
}
