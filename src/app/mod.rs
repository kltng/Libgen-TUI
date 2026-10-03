pub mod config;
pub mod downloads;
pub mod focus;

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::{
    sync::Semaphore,
    task::{JoinHandle, JoinSet},
};

use ratatui::widgets::TableState;
use reqwest::Client;
use tui_textarea::TextArea;

pub use config::AppConfig;
pub use downloads::{Download, DownloadStatus, Downloads};
pub use focus::Focus;

use crate::libgen::{self, download, search, Book};

type SearchResult = Result<(Vec<Book>, String), search::SearchError>;
pub const MAX_CONCURRENT_DOWNLOADS: usize = 3;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResultsState {
    Idle,
    Searching,
    QueryTooShort,
    NoResults,
    Failed(String),
    Results,
}

pub struct App {
    pub client: Client,
    pub download_client: Client,
    search_task: Option<JoinHandle<SearchResult>>,
    download_tasks: JoinSet<()>,
    download_slots: Arc<Semaphore>,
    pub config: AppConfig,
    pub downloads: Downloads,
    pub mirrors: Vec<String>,
    pub active_mirror: Option<String>,

    pub search_bar: TextArea<'static>,
    pub search_results: Vec<Book>,
    pub table_state: TableState,
    pub results_state: ResultsState,

    pub focus: Focus,
    pub show_popup: bool,
    pub should_quit: bool,
}

impl App {
    pub fn new(config: AppConfig) -> Self {
        let download_dir = PathBuf::from(&config.download_directory);
        if !download_dir.exists() {
            fs::create_dir_all(&download_dir)
                .expect("Failed to create directory to install files.");
        }

        App {
            client: libgen::build_client(),
            download_client: libgen::build_download_client(),
            search_task: None,
            download_tasks: JoinSet::new(),
            download_slots: Arc::new(Semaphore::new(MAX_CONCURRENT_DOWNLOADS)),
            config,
            downloads: Downloads::new(),
            mirrors: config::default_mirrors(),
            active_mirror: None,
            search_bar: TextArea::default(),
            search_results: Vec::new(),
            table_state: TableState::default(),
            results_state: ResultsState::Idle,
            focus: Focus::SearchBar,
            show_popup: false,
            should_quit: false,
        }
    }

    pub fn query(&self) -> String {
        self.search_bar.lines().join(" ").trim().to_string()
    }

    pub fn selected_book(&self) -> Option<&Book> {
        if self.results_state != ResultsState::Results {
            return None;
        }
        self.table_state
            .selected()
            .and_then(|index| self.search_results.get(index))
    }

    pub fn select_next(&mut self) {
        if let Some(index) = self.table_state.selected() {
            if index + 1 < self.search_results.len() {
                self.table_state.select(Some(index + 1));
            }
        }
    }

    pub fn select_previous(&mut self) {
        if let Some(index) = self.table_state.selected() {
            if index > 0 {
                self.table_state.select(Some(index - 1));
            }
        }
    }

    pub fn set_results(&mut self, results: Vec<Book>) {
        if results.is_empty() {
            self.table_state.select(None);
            self.results_state = ResultsState::NoResults;
            self.focus = Focus::SearchBar;
        } else {
            self.table_state.select(Some(0));
            self.results_state = ResultsState::Results;
            self.focus = Focus::Table;
        }

        self.search_results = results;
    }

    pub fn clear_results(&mut self, state: ResultsState) {
        if let Some(task) = self.search_task.take() {
            task.abort();
        }
        self.search_results.clear();
        self.table_state.select(None);
        self.show_popup = false;
        self.results_state = state;
    }

    pub fn start_search(&mut self, query: String) {
        self.clear_results(ResultsState::Searching);
        let Some(preferred) = self.active_mirror.clone() else {
            self.results_state = ResultsState::Failed("no mirror is reachable".into());
            self.focus = Focus::SearchBar;
            return;
        };
        let client = self.client.clone();
        let mirrors = self.mirrors.clone();
        let max_results = self.config.max_results;
        self.search_task = Some(tokio::spawn(async move {
            search::search(&client, &mirrors, &preferred, &query, max_results).await
        }));
    }

    pub async fn poll_background(&mut self) {
        if self
            .search_task
            .as_ref()
            .is_some_and(|task| task.is_finished())
        {
            let task = self.search_task.take().expect("finished search task");
            match task.await {
                Ok(Ok((books, mirror))) => {
                    self.active_mirror = Some(mirror);
                    self.set_results(books);
                }
                result => {
                    let error = match result {
                        Ok(Err(e)) => e.to_string(),
                        Err(e) => e.to_string(),
                        _ => unreachable!(),
                    };
                    self.clear_results(ResultsState::Failed(error));
                    self.focus = Focus::SearchBar;
                }
            }
        }
        while self.download_tasks.try_join_next().is_some() {}
    }

    pub async fn shutdown(&mut self) {
        if let Some(task) = self.search_task.take() {
            task.abort();
            let _ = task.await;
        }
        self.download_tasks.abort_all();
        while self.download_tasks.join_next().await.is_some() {}
    }

    pub fn start_selected_download(&mut self) {
        let (Some(mirror), Some(mut book)) =
            (self.active_mirror.clone(), self.selected_book().cloned())
        else {
            return;
        };
        book.md5.make_ascii_lowercase();
        if !self.downloads.start(&book.title, &book.md5) {
            return;
        }
        let destination = match download::book_destination(&self.config.download_directory, &book) {
            Ok(path) => path,
            Err(e) => {
                self.downloads.fail(&book.md5, e);
                return;
            }
        };
        let client = self.client.clone();
        let transfer_client = self.download_client.clone();
        let mirrors = self.mirrors.clone();
        let downloads = self.downloads.clone();
        let slots = self.download_slots.clone();
        self.download_tasks.spawn(async move {
            let _permit = slots
                .acquire_owned()
                .await
                .expect("download semaphore stays open");
            match download::download_book(
                &client,
                &transfer_client,
                &mirrors,
                &mirror,
                &book,
                &destination,
            )
            .await
            {
                Ok(()) => downloads.complete(&book.md5),
                Err(e) => {
                    log::error!("Download of {} failed: {}", book.title, e);
                    downloads.fail(&book.md5, e);
                }
            }
        });
    }
}

impl Drop for App {
    fn drop(&mut self) {
        if let Some(task) = self.search_task.take() {
            task.abort();
        }
    }
}
