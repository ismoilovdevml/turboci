//! A stand-in for dockerd on a unix socket, for tests of how the client
//! uses its connections: it counts them, records every request, and can
//! hang up or restart like a dockerd that goes away.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::task::JoinHandle;

/// How the fake answers a request
pub enum Answer {
    /// 200 with this JSON body; the connection stays open for more
    Json(&'static str),
    /// 101 Switching Protocols, then these bytes of the hijacked stream,
    /// then the connection is closed
    Upgrade(Vec<u8>),
    /// The connection is closed without an answer
    HangUp,
}

/// Picks the answer to `METHOD /path` (query left out), the `n`th request
/// (from 1) on its connection
type Answers = dyn Fn(&str, usize) -> Answer + Send + Sync;

pub struct FakeDockerd {
    _dir: tempfile::TempDir,
    path: PathBuf,
    answers: Arc<Answers>,
    connections: Arc<AtomicUsize>,
    /// Each request as (connection number from 1, `METHOD /path`)
    requests: Arc<Mutex<Vec<(usize, String)>>>,
    /// The accept loop and every connection's task
    tasks: Arc<Mutex<Vec<JoinHandle<()>>>>,
}

impl FakeDockerd {
    pub fn start(answers: impl Fn(&str, usize) -> Answer + Send + Sync + 'static) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let fake = Self {
            path: dir.path().join("docker.sock"),
            _dir: dir,
            answers: Arc::new(answers),
            connections: Arc::default(),
            requests: Arc::default(),
            tasks: Arc::default(),
        };
        fake.listen();
        fake
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Connections accepted so far
    pub fn connections(&self) -> usize {
        self.connections.load(Ordering::SeqCst)
    }

    pub fn requests(&self) -> Vec<(usize, String)> {
        self.requests.lock().unwrap().clone()
    }

    /// Close every connection and the socket, then listen again, as a
    /// restarted dockerd does
    pub fn restart(&self) {
        for task in self.tasks.lock().unwrap().drain(..) {
            task.abort();
        }
        let _ = std::fs::remove_file(&self.path);
        self.listen();
    }

    fn listen(&self) {
        let listener = UnixListener::bind(&self.path).unwrap();
        let answers = Arc::clone(&self.answers);
        let connections = Arc::clone(&self.connections);
        let requests = Arc::clone(&self.requests);
        let tasks = Arc::clone(&self.tasks);
        let accept = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let number = connections.fetch_add(1, Ordering::SeqCst) + 1;
                let serve = serve(stream, number, Arc::clone(&answers), Arc::clone(&requests));
                tasks.lock().unwrap().push(tokio::spawn(serve));
            }
        });
        self.tasks.lock().unwrap().push(accept);
    }
}

impl Drop for FakeDockerd {
    fn drop(&mut self) {
        for task in self.tasks.lock().unwrap().drain(..) {
            task.abort();
        }
    }
}

/// Answer the requests of one connection until the client or an answer ends it
async fn serve(
    mut stream: UnixStream,
    number: usize,
    answers: Arc<Answers>,
    requests: Arc<Mutex<Vec<(usize, String)>>>,
) {
    let mut buffer = Vec::new();
    let mut served = 0;
    loop {
        // The head, then a body of its Content-Length
        let head_end = loop {
            if let Some(at) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
                break at + 4;
            }
            let mut chunk = [0u8; 4096];
            match stream.read(&mut chunk).await {
                Ok(0) | Err(_) => return,
                Ok(n) => buffer.extend_from_slice(&chunk[..n]),
            }
        };
        let head = String::from_utf8_lossy(&buffer[..head_end]).to_string();
        let length: usize = head
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse().ok())?
            })
            .unwrap_or(0);
        while buffer.len() < head_end + length {
            let mut chunk = [0u8; 4096];
            match stream.read(&mut chunk).await {
                Ok(0) | Err(_) => return,
                Ok(n) => buffer.extend_from_slice(&chunk[..n]),
            }
        }
        buffer.drain(..head_end + length);
        let mut words = head.split_whitespace();
        let method = words.next().unwrap_or_default();
        let target = words.next().unwrap_or_default();
        let request = format!(
            "{} {}",
            method,
            target.split('?').next().unwrap_or_default()
        );
        requests.lock().unwrap().push((number, request.clone()));
        served += 1;
        match answers(&request, served) {
            Answer::Json(body) => {
                let answer = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                    body.len(),
                    body
                );
                if stream.write_all(answer.as_bytes()).await.is_err() {
                    return;
                }
            }
            Answer::Upgrade(output) => {
                let head = "HTTP/1.1 101 UPGRADED\r\nContent-Type: application/vnd.docker.raw-stream\r\nConnection: Upgrade\r\nUpgrade: tcp\r\n\r\n";
                let _ = stream.write_all(head.as_bytes()).await;
                let _ = stream.write_all(&output).await;
                let _ = stream.shutdown().await;
                return;
            }
            Answer::HangUp => return,
        }
    }
}
