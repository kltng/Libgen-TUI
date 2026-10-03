use std::path::{Path, PathBuf};

use md5::{Digest, Md5};
use reqwest::Client;
use scraper::{Html, Selector};
use tokio::io::{AsyncWriteExt, BufWriter};

use super::{mirrors_by_preference, Book};

#[derive(Debug, thiserror::Error)]
pub enum DownloadError {
    #[error("request failed: {0}")]
    Request(#[from] reqwest::Error),
    #[error("invalid download URL: {0}")]
    Url(#[from] url::ParseError),
    #[error("no download link on the page")]
    LinkNotFound,
    #[error("invalid book checksum")]
    InvalidChecksum,
    #[error("download checksum did not match the selected book")]
    ChecksumMismatch,
    #[error("could not write {path}: {source}")]
    Write {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("mirror served a web page or empty response instead of the file")]
    NotAFile,
}

pub fn parse_download_href(body: &str) -> Option<String> {
    let document = Html::parse_document(body);
    let selector = Selector::parse("a[href*=\"get.php\"]").unwrap();
    document
        .select(&selector)
        .find_map(|a| a.value().attr("href"))
        .map(str::to_string)
}

pub async fn resolve_url(
    client: &Client,
    mirror: &str,
    md5: &str,
) -> Result<String, DownloadError> {
    let mut url = super::mirror_url(mirror, "/ads.php")?;
    url.query_pairs_mut().append_pair("md5", md5);
    let response = client.get(url).send().await?.error_for_status()?;
    let base = response.url().clone();
    let body = response.text().await?;
    let href = parse_download_href(&body).ok_or(DownloadError::LinkNotFound)?;
    let url = base.join(&href)?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(DownloadError::LinkNotFound);
    }
    Ok(url.into())
}

pub async fn resolve_url_with_failover(
    client: &Client,
    mirrors: &[String],
    preferred: &str,
    md5: &str,
) -> Result<String, DownloadError> {
    let mut error = DownloadError::LinkNotFound;
    for mirror in mirrors_by_preference(mirrors, preferred) {
        match resolve_url(client, &mirror, md5).await {
            Ok(url) => return Ok(url),
            Err(e) => error = e,
        }
    }
    Err(error)
}

/// Retry the complete operation, including the transfer and content validation.
/// Local filesystem errors are terminal: changing mirrors cannot fix them.
pub async fn download_book(
    metadata_client: &Client,
    transfer_client: &Client,
    mirrors: &[String],
    preferred: &str,
    book: &Book,
    destination: &Path,
) -> Result<(), DownloadError> {
    validate_checksum(&book.md5)?;
    let mut error = DownloadError::LinkNotFound;
    for mirror in mirrors_by_preference(mirrors, preferred) {
        let result = async {
            let url = resolve_url(metadata_client, &mirror, &book.md5).await?;
            transfer(transfer_client, &url, destination, Some(&book.md5)).await
        }
        .await;
        match result {
            Ok(()) => return Ok(()),
            Err(e @ DownloadError::Write { .. }) => return Err(e),
            Err(e) => {
                log::warn!("Download from {} failed: {}", mirror, e);
                error = e;
            }
        }
    }
    Err(error)
}

fn validate_checksum(checksum: &str) -> Result<(), DownloadError> {
    if checksum.len() == 32 && checksum.bytes().all(|b| b.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err(DownloadError::InvalidChecksum)
    }
}

pub fn sanitize_filename(title: &str, extension: &str) -> String {
    // Bound the UTF-8 byte length as well as excluding separators and Windows
    // metacharacters. Leave room for the identifier and extension on disk.
    let mut stem = String::new();
    for c in title.chars() {
        let c = match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if c.is_control() || c.is_whitespace() => '_',
            c => c,
        };
        if stem.len() + c.len_utf8() > 120 {
            break;
        }
        stem.push(c);
    }
    let stem = stem.trim_matches(['.', '_']);
    let stem = if stem.is_empty() { "book" } else { stem };
    let base = stem.split('.').next().unwrap_or(stem).to_uppercase();
    let reserved = matches!(
        base.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) || ["COM", "LPT"].iter().any(|prefix| {
        base.strip_prefix(prefix).is_some_and(|suffix| {
            matches!(
                suffix,
                "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
            )
        })
    });
    let stem = if reserved {
        format!("_{stem}")
    } else {
        stem.to_string()
    };
    // Never interpret remote extension text as part of a path.
    let extension = extension.trim();
    let extension = if extension.len() <= 16 && extension.bytes().all(|b| b.is_ascii_alphanumeric())
    {
        extension
    } else {
        "bin"
    };
    if extension.is_empty() {
        stem
    } else {
        format!("{stem}.{}", extension.to_ascii_lowercase())
    }
}

pub fn destination_path(directory: &str, title: &str, extension: &str) -> PathBuf {
    Path::new(directory).join(sanitize_filename(title, extension))
}

pub fn book_destination(directory: &str, book: &Book) -> Result<PathBuf, DownloadError> {
    validate_checksum(&book.md5)?;
    let title = sanitize_filename(&book.title, "");
    let extension = sanitize_filename("book", &book.extension);
    let extension = extension.strip_prefix("book").unwrap_or("");
    Ok(Path::new(directory).join(format!(
        "{title}-{}{extension}",
        book.md5.to_ascii_lowercase()
    )))
}

pub async fn download_to_file(
    client: &Client,
    url: &str,
    destination: &Path,
) -> Result<(), DownloadError> {
    transfer(client, url, destination, None).await
}

fn looks_like_html(prefix: &[u8]) -> bool {
    let text = String::from_utf8_lossy(prefix).to_ascii_lowercase();
    let mut text = text.trim_start_matches('\u{feff}').trim_start();
    loop {
        let terminator = if text.starts_with("<!--") {
            "-->"
        } else if text.starts_with("<?xml") {
            "?>"
        } else {
            break;
        };
        let Some(end) = text.find(terminator) else {
            return false;
        };
        text = text[end + terminator.len()..].trim_start();
    }
    text.starts_with("<!doctype html")
        || text.starts_with("<html")
        || text.starts_with("<head")
        || text.starts_with("<body")
}

async fn transfer(
    client: &Client,
    url: &str,
    destination: &Path,
    checksum: Option<&str>,
) -> Result<(), DownloadError> {
    let write_error = |source| DownloadError::Write {
        path: destination.display().to_string(),
        source,
    };
    // Fail before network I/O if a file, directory or symlink already occupies
    // the name. persist_noclobber below also protects against concurrent writers.
    match std::fs::symlink_metadata(destination) {
        Ok(_) => {
            return Err(write_error(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "destination already exists",
            )))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(write_error(e)),
    }
    let mut response = client.get(url).send().await?.error_for_status()?;
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    if content_type.starts_with("text/html") || content_type.starts_with("application/xhtml+xml") {
        return Err(DownloadError::NotAFile);
    }

    let parent = destination
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let temp = tempfile::Builder::new()
        .prefix(".libgen-")
        .tempfile_in(parent)
        .map_err(write_error)?;
    // TempPath removes unfinished data on errors and task cancellation. Drop the
    // writer before publishing so the same sequence works on Windows too.
    let (file, path) = temp.into_parts();
    let mut writer = BufWriter::new(tokio::fs::File::from_std(file));
    let mut prefix = Vec::with_capacity(8192);
    let mut digest = Md5::new();
    let mut size = 0u64;
    while let Some(chunk) = response.chunk().await? {
        prefix.extend_from_slice(&chunk[..chunk.len().min(8192 - prefix.len())]);
        if looks_like_html(&prefix) {
            return Err(DownloadError::NotAFile);
        }
        size += chunk.len() as u64;
        digest.update(&chunk);
        writer.write_all(&chunk).await.map_err(write_error)?;
    }
    if size == 0 {
        return Err(DownloadError::NotAFile);
    }
    if let Some(expected) = checksum {
        if !format!("{:x}", digest.finalize()).eq_ignore_ascii_case(expected) {
            return Err(DownloadError::ChecksumMismatch);
        }
    }
    writer.flush().await.map_err(write_error)?;
    writer.get_ref().sync_all().await.map_err(write_error)?;
    drop(writer);
    path.persist_noclobber(destination)
        .map_err(|e| write_error(e.error))?;
    Ok(())
}
