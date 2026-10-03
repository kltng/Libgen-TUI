use std::{
    io::{Read, Write},
    net::TcpListener,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread::{self, JoinHandle},
    time::Duration,
};

pub struct Reply {
    pub status: u16,
    pub content_type: Option<&'static str>,
    pub body: Vec<u8>,
    pub chunk_delay: Duration,
    pub chunk_size: usize,
    pub declared_length: Option<usize>,
}
impl Reply {
    pub fn ok(body: impl Into<Vec<u8>>) -> Self {
        Self {
            status: 200,
            content_type: None,
            body: body.into(),
            chunk_delay: Duration::ZERO,
            chunk_size: 1,
            declared_length: None,
        }
    }
}
pub struct Server {
    pub url: String,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}
impl Server {
    pub fn new(handler: impl Fn(&str) -> Reply + Send + Sync + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let stop = Arc::new(AtomicBool::new(false));
        let done = stop.clone();
        let handler = Arc::new(handler);
        let thread = thread::spawn(move || {
            while !done.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut socket, _)) => {
                        // BSD/macOS accept inherits the listener's nonblocking flag.
                        socket.set_nonblocking(false).unwrap();
                        let handler = handler.clone();
                        thread::spawn(move || {
                            socket
                                .set_read_timeout(Some(Duration::from_secs(3)))
                                .unwrap();
                            socket
                                .set_write_timeout(Some(Duration::from_secs(3)))
                                .unwrap();
                            let mut request = Vec::new();
                            let mut buffer = [0u8; 1024];
                            while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                                match socket.read(&mut buffer) {
                                    Ok(0) | Err(_) => return,
                                    Ok(n) => request.extend_from_slice(&buffer[..n]),
                                }
                                if request.len() > 16384 {
                                    return;
                                }
                            }
                            let request = String::from_utf8_lossy(&request);
                            let path = request.split_whitespace().nth(1).unwrap_or("/");
                            let reply = handler(path);
                            let content_type = reply
                                .content_type
                                .map(|c| format!("Content-Type: {c}\r\n"))
                                .unwrap_or_default();
                            if write!(socket, "HTTP/1.1 {} Test\r\nContent-Length: {}\r\n{}Connection: close\r\n\r\n", reply.status, reply.declared_length.unwrap_or(reply.body.len()), content_type).is_err() { return; }
                            if reply.chunk_delay.is_zero() {
                                let _ = socket.write_all(&reply.body);
                            } else {
                                for chunk in reply.body.chunks(reply.chunk_size) {
                                    if socket.write_all(chunk).is_err() {
                                        break;
                                    }
                                    thread::sleep(reply.chunk_delay);
                                }
                            }
                        });
                    }
                    Err(_) => thread::sleep(Duration::from_millis(2)),
                }
            }
        });
        Self {
            url,
            stop,
            thread: Some(thread),
        }
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.thread.take().unwrap().join().unwrap();
    }
}
