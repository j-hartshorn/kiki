use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio::time::timeout;

#[derive(Clone)]
pub(crate) enum FixtureReply {
    Sse(&'static str),
    OwnedSse(String),
    Status {
        code: u16,
        body: &'static str,
    },
    OwnedStatus {
        code: u16,
        headers: Vec<(String, String)>,
        body: String,
    },
    AbruptSse(&'static str),
    CloseBeforeHeaders,
    IdleSse(&'static str),
}

#[derive(Clone)]
pub(crate) struct CapturedRequest {
    pub(crate) head: String,
    pub(crate) body: Value,
}

pub(crate) struct Fixture {
    pub(crate) endpoint: String,
    pub(crate) requests: Arc<Mutex<Vec<CapturedRequest>>>,
    requests_received: Arc<AtomicUsize>,
    request_received: Arc<Notify>,
    replies_written: Arc<AtomicUsize>,
    reply_written: Arc<Notify>,
    disconnected: Arc<AtomicBool>,
    task: JoinHandle<()>,
}

impl Fixture {
    pub(crate) async fn start(reply: FixtureReply) -> Self {
        Self::start_sequence(vec![reply]).await
    }

    pub(crate) async fn start_sequence(replies: Vec<FixtureReply>) -> Self {
        assert!(!replies.is_empty());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let requests_received = Arc::new(AtomicUsize::new(0));
        let request_received = Arc::new(Notify::new());
        let replies_written = Arc::new(AtomicUsize::new(0));
        let reply_written = Arc::new(Notify::new());
        let disconnected = Arc::new(AtomicBool::new(false));
        let task = {
            let requests = requests.clone();
            let requests_received = requests_received.clone();
            let request_received = request_received.clone();
            let replies_written = replies_written.clone();
            let reply_written = reply_written.clone();
            let disconnected = disconnected.clone();
            tokio::spawn(async move {
                let mut reply_index = 0;
                loop {
                    let (mut stream, _) = listener.accept().await.unwrap();
                    let request = read_request(&mut stream).await;
                    requests.lock().unwrap().push(request);
                    requests_received.fetch_add(1, Ordering::SeqCst);
                    request_received.notify_waiters();
                    let reply = replies
                        .get(reply_index)
                        .or_else(|| replies.last())
                        .expect("fixture reply");
                    reply_index += 1;
                    write_reply(&mut stream, reply, &disconnected).await;
                    replies_written.fetch_add(1, Ordering::SeqCst);
                    reply_written.notify_waiters();
                }
            })
        };
        Self {
            endpoint,
            requests,
            requests_received,
            request_received,
            replies_written,
            reply_written,
            disconnected,
            task,
        }
    }

    pub(crate) fn request_count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }

    pub(crate) fn request(&self) -> CapturedRequest {
        self.requests.lock().unwrap()[0].clone()
    }

    pub(crate) async fn wait_for_requests(&self, count: usize) {
        timeout(Duration::from_secs(1), async {
            loop {
                let notified = self.request_received.notified();
                if self.requests_received.load(Ordering::SeqCst) >= count {
                    break;
                }
                notified.await;
            }
        })
        .await
        .expect("fixture should receive a request");
    }

    pub(crate) async fn wait_for_replies_written(&self, count: usize) {
        timeout(Duration::from_secs(1), async {
            loop {
                let notified = self.reply_written.notified();
                if self.replies_written.load(Ordering::SeqCst) >= count {
                    break;
                }
                notified.await;
            }
        })
        .await
        .expect("fixture should finish writing its reply");
    }

    pub(crate) async fn wait_for_disconnect(&self) {
        timeout(Duration::from_secs(1), async {
            while !self.disconnected.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("dropping the SDK stream should close the fixture connection");
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn read_request(stream: &mut TcpStream) -> CapturedRequest {
    let mut bytes = Vec::new();
    let mut buffer = [0; 4096];
    let header_end = loop {
        let read = stream.read(&mut buffer).await.unwrap();
        assert!(read > 0, "fixture connection closed before request headers");
        bytes.extend_from_slice(&buffer[..read]);
        if let Some(position) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            break position + 4;
        }
    };
    let head = String::from_utf8(bytes[..header_end].to_vec()).unwrap();
    let content_length = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().unwrap())
        })
        .unwrap();
    while bytes.len() < header_end + content_length {
        let read = stream.read(&mut buffer).await.unwrap();
        assert!(read > 0, "fixture connection closed before request body");
        bytes.extend_from_slice(&buffer[..read]);
    }
    let body = serde_json::from_slice(&bytes[header_end..header_end + content_length]).unwrap();
    CapturedRequest { head, body }
}

async fn write_reply(stream: &mut TcpStream, reply: &FixtureReply, disconnected: &AtomicBool) {
    match reply {
        FixtureReply::Sse(body) => {
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        }
        FixtureReply::OwnedSse(body) => {
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        }
        FixtureReply::Status { code, body } => {
            let response = format!(
                "HTTP/1.1 {code} Fixture Error\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        }
        FixtureReply::OwnedStatus {
            code,
            headers,
            body,
        } => {
            let mut response = format!(
                "HTTP/1.1 {code} Fixture Error\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n",
                body.len()
            );
            for (name, value) in headers {
                response.push_str(name);
                response.push_str(": ");
                response.push_str(value);
                response.push_str("\r\n");
            }
            response.push_str("\r\n");
            response.push_str(body);
            stream.write_all(response.as_bytes()).await.unwrap();
        }
        FixtureReply::AbruptSse(body) => {
            write_chunked_headers(stream).await;
            write_chunk(stream, body).await;
        }
        FixtureReply::CloseBeforeHeaders => stream.shutdown().await.unwrap(),
        FixtureReply::IdleSse(body) => {
            write_chunked_headers(stream).await;
            write_chunk(stream, body).await;
            let mut byte = [0];
            if stream.read(&mut byte).await.unwrap_or(0) == 0 {
                disconnected.store(true, Ordering::SeqCst);
            }
        }
    }
}

async fn write_chunked_headers(stream: &mut TcpStream) {
    stream
        .write_all(
            b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n",
        )
        .await
        .unwrap();
}

async fn write_chunk(stream: &mut TcpStream, body: &str) {
    stream
        .write_all(format!("{:X}\r\n{}\r\n", body.len(), body).as_bytes())
        .await
        .unwrap();
}
