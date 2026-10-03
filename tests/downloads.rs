mod support;

use libgen_tui::libgen::{build_client, build_download_client, download::*, Book};
use md5::{Digest, Md5};
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use support::{Reply, Server};

fn book(body: &[u8]) -> Book {
    Book {
        title: "A/B".into(),
        extension: "pdf".into(),
        md5: format!("{:x}", Md5::digest(body)),
        ..Default::default()
    }
}
fn entries(dir: &std::path::Path) -> usize {
    std::fs::read_dir(dir).unwrap().count()
}

#[test]
fn paths_are_single_bounded_components_and_distinguish_editions() {
    let dir = tempfile::tempdir().unwrap();
    for title in ["../../escape", "CON", "AUX.txt", "...", "", "😀/書"] {
        for extension in ["d/../../victim", "\\..\\victim", "pdf", "", "foo:bar"] {
            let path = destination_path(dir.path().to_str().unwrap(), title, extension);
            assert_eq!(path.parent(), Some(dir.path()));
            assert!(path.file_name().unwrap().to_str().unwrap().len() < 200);
            assert!(!path
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .contains(['/', '\\', ':']));
        }
    }
    assert_eq!(sanitize_filename("CON", "pdf"), "_CON.pdf");
    let mut a = book(b"edition one");
    let mut b = book(b"edition two");
    b.title = "A:B".into();
    assert_ne!(
        book_destination("books", &a).unwrap(),
        book_destination("books", &b).unwrap()
    );
    a.title = "😀".repeat(200);
    assert!(
        book_destination("books", &a)
            .unwrap()
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .len()
            < 200
    );
    a.md5 = "../../escape".into();
    assert!(book_destination("books", &a).is_err());
}

#[tokio::test]
async fn existing_files_are_preserved_and_concurrent_publish_has_one_winner() {
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("book.pdf");
    let server = Server::new(|_| Reply::ok(b"new data".to_vec()));
    let client = build_download_client();
    std::fs::write(&dest, b"original").unwrap();
    assert!(matches!(
        download_to_file(&client, &server.url, &dest).await,
        Err(DownloadError::Write { .. })
    ));
    assert_eq!(std::fs::read(&dest).unwrap(), b"original");
    std::fs::remove_file(&dest).unwrap();
    let (a, b) = tokio::join!(
        download_to_file(&client, &server.url, &dest),
        download_to_file(&client, &server.url, &dest)
    );
    assert_ne!(a.is_ok(), b.is_ok(), "a={a:?}; b={b:?}");
    assert_eq!(std::fs::read(dest).unwrap(), b"new data");
    assert_eq!(entries(dir.path()), 1);
}

#[tokio::test]
async fn html_empty_and_truncated_responses_leave_no_files() {
    for mode in 0..5 {
        let server = Server::new(move |_| {
            let mut r = Reply::ok(b"<HTML>Access denied</HTML>".to_vec());
            match mode {
                0 => r.chunk_delay = Duration::from_millis(1), // signature split across chunks
                1 => {
                    r.body = b"not a book".to_vec();
                    r.content_type = Some("Text/HTML; charset=utf-8");
                }
                2 => r.body.clear(),
                3 => {
                    r.body = b"partial".to_vec();
                    r.declared_length = Some(100);
                }
                _ => r.content_type = Some("application/xhtml+xml"),
            }
            r
        });
        let dir = tempfile::tempdir().unwrap();
        assert!(download_to_file(
            &build_download_client(),
            &server.url,
            &dir.path().join("book.pdf")
        )
        .await
        .is_err());
        assert_eq!(entries(dir.path()), 0);
    }
}

#[tokio::test]
async fn retries_transfer_errors_and_checksum_mismatches_on_other_mirrors() {
    for mode in 0..3 {
        let bad = Server::new(move |path| {
            if path.starts_with("/ads.php") {
                return Reply::ok(b"<a href='/get.php'>Get</a>".to_vec());
            }
            let mut reply = Reply::ok(b"wrong book".to_vec());
            if mode == 0 {
                reply.status = 503;
            }
            if mode == 1 {
                reply.body = b"<html>blocked</html>".to_vec();
            }
            reply
        });
        let good = Server::new(|path| {
            if path.starts_with("/ads.php") {
                Reply::ok(b"<a href='/get.php'>Get</a>".to_vec())
            } else {
                Reply::ok(b"correct book".to_vec())
            }
        });
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("book.pdf");
        download_book(
            &build_client(),
            &build_download_client(),
            &[bad.url.clone(), good.url.clone()],
            &bad.url,
            &book(b"correct book"),
            &dest,
        )
        .await
        .unwrap();
        assert_eq!(std::fs::read(dest).unwrap(), b"correct book");
        assert_eq!(entries(dir.path()), 1);
    }
}

#[tokio::test]
async fn local_write_errors_do_not_retry_other_mirrors() {
    let count = Arc::new(AtomicUsize::new(0));
    let hits = count.clone();
    let first = Server::new(|_| Reply::ok(b"<a href='/get.php'>Get</a>".to_vec()));
    let second = Server::new(move |_| {
        hits.fetch_add(1, Ordering::SeqCst);
        Reply::ok(vec![])
    });
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("book.pdf");
    std::fs::write(&dest, b"existing").unwrap();
    let result = download_book(
        &build_client(),
        &build_download_client(),
        std::slice::from_ref(&second.url),
        &first.url,
        &book(b"book"),
        &dest,
    )
    .await;
    assert!(matches!(result, Err(DownloadError::Write { .. })));
    assert_eq!(count.load(Ordering::SeqCst), 0);
    assert_eq!(std::fs::read(dest).unwrap(), b"existing");
}

#[tokio::test]
async fn cancellation_removes_partial_file() {
    let server = Server::new(|_| {
        let mut r = Reply::ok(vec![42; 100]);
        r.chunk_delay = Duration::from_millis(10);
        r
    });
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("book.pdf");
    let task = tokio::spawn(async move {
        download_to_file(&build_download_client(), &server.url, &dest).await
    });
    tokio::time::timeout(Duration::from_secs(2), async {
        while entries(dir.path()) == 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    task.abort();
    let _ = task.await;
    assert_eq!(entries(dir.path()), 0);
}

#[tokio::test]
async fn healthy_transfer_can_last_longer_than_thirty_seconds() {
    let server = Server::new(|_| {
        let mut r = Reply::ok(vec![42; 33]);
        r.chunk_delay = Duration::from_secs(1);
        r
    });
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("book.bin");
    download_to_file(&build_download_client(), &server.url, &dest)
        .await
        .unwrap();
    assert_eq!(std::fs::read(dest).unwrap(), vec![42; 33]);
}

#[tokio::test]
async fn embedded_html_text_in_a_binary_book_is_not_an_error_page() {
    let body = b"%PDF-1.7\nmetadata mentioning <html> is valid book content";
    let server = Server::new(move |_| Reply::ok(body.to_vec()));
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("book.pdf");
    download_to_file(&build_download_client(), &server.url, &dest)
        .await
        .unwrap();
    assert_eq!(std::fs::read(dest).unwrap(), body);
}

#[tokio::test]
async fn malicious_parsed_extension_cannot_overwrite_outside_download_directory() {
    let dir = tempfile::tempdir().unwrap();
    let downloads = dir.path().join("downloads");
    std::fs::create_dir_all(downloads.join("archive.d")).unwrap();
    let victim = dir.path().join("victim.txt");
    std::fs::write(&victim, b"preserve me").unwrap();
    let body = b"legitimate bytes";
    let md5 = format!("{:x}", Md5::digest(body));
    let html = format!("<table id='tablelibgen'><tbody><tr><td><a>archive</a></td><td>A</td><td>P</td><td>2020</td><td>English</td><td>1</td><td>1 KB</td><td>d/../../victim.txt</td><td><a href='ads.php?md5={md5}'>get</a></td></tr></tbody></table>");
    let books = libgen_tui::libgen::search::parse_books(&html);
    let destination = book_destination(downloads.to_str().unwrap(), &books[0]).unwrap();
    let server = Server::new(move |path| {
        if path.starts_with("/ads.php") {
            Reply::ok("<a href='get.php'>get</a>")
        } else {
            Reply::ok(body.to_vec())
        }
    });
    download_book(
        &build_client(),
        &build_download_client(),
        &[],
        &server.url,
        &books[0],
        &destination,
    )
    .await
    .unwrap();
    assert_eq!(std::fs::read(victim).unwrap(), b"preserve me");
    assert_eq!(std::fs::read(destination).unwrap(), body);
}

#[tokio::test]
async fn idle_transfer_times_out_and_cleans_up() {
    let server = Server::new(|_| {
        let mut reply = Reply::ok(b"slow".to_vec());
        reply.chunk_delay = Duration::from_millis(200);
        reply
    });
    let client = reqwest::Client::builder()
        .read_timeout(Duration::from_millis(50))
        .build()
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let error = download_to_file(&client, &server.url, &dir.path().join("book.pdf"))
        .await
        .unwrap_err();
    assert!(matches!(error, DownloadError::Request(e) if e.is_timeout()));
    assert_eq!(entries(dir.path()), 0);
}

#[tokio::test]
async fn streams_to_disk_before_response_finishes_and_publishes_only_at_end() {
    let body = vec![42; 512 * 1024];
    let served = body.clone();
    let server = Server::new(move |_| {
        let mut reply = Reply::ok(served.clone());
        reply.chunk_size = 16 * 1024;
        reply.chunk_delay = Duration::from_millis(20);
        reply
    });
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("book.bin");
    let output = dest.clone();
    let task = tokio::spawn(async move {
        download_to_file(&build_download_client(), &server.url, &output).await
    });
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let written = std::fs::read_dir(dir.path())
                .unwrap()
                .filter_map(Result::ok)
                .any(|e| {
                    e.file_name().to_string_lossy().starts_with(".libgen-")
                        && e.metadata().unwrap().len() >= 16384
                });
            if written {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert!(!dest.exists());
    assert!(!task.is_finished());
    task.await.unwrap().unwrap();
    assert_eq!(std::fs::read(dest).unwrap(), body);
    assert_eq!(entries(dir.path()), 1);
}

#[tokio::test]
async fn resolves_protocol_relative_links_using_the_response_origin() {
    let server = Server::new(|_| Reply::ok("<a href='//example.test/get.php?key=abc'>get</a>"));
    let resolved = resolve_url(&build_client(), &server.url, "abc")
        .await
        .unwrap();
    assert_eq!(resolved, "http://example.test/get.php?key=abc");
}
