pub mod download;
pub mod mirror;
pub mod search;

use std::time::Duration;

use reqwest::Client;

pub use search::Book;

pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

pub fn build_client() -> Client {
    Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()
        .expect("Failed to build http client.")
}

/// File transfers have a per-read deadline, not a deadline for the whole book.
pub fn build_download_client() -> Client {
    Client::builder()
        .connect_timeout(REQUEST_TIMEOUT)
        .read_timeout(REQUEST_TIMEOUT)
        .build()
        .expect("Failed to build download client.")
}

/// Bare mirror domains use HTTPS. Explicit URLs support private/local mirrors.
pub fn mirror_url(mirror: &str, path: &str) -> Result<reqwest::Url, url::ParseError> {
    let base = if mirror.contains("://") {
        mirror.to_string()
    } else {
        format!("https://{mirror}")
    };
    reqwest::Url::parse(&base)?.join(path)
}

pub fn mirrors_by_preference(mirrors: &[String], preferred: &str) -> Vec<String> {
    let mut ordered = vec![preferred.to_string()];
    ordered.extend(
        mirrors
            .iter()
            .filter(|mirror| mirror.as_str() != preferred)
            .cloned(),
    );
    ordered
}
