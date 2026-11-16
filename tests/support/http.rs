use std::{
    collections::HashMap,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

pub struct Request {
    #[allow(dead_code)] // Not every fixture needs to distinguish GET and HEAD.
    pub method: String,
    pub path: String,
    pub headers: HashMap<String, String>,
}
pub struct Response {
    pub status: u16,
    pub body: Vec<u8>,
    pub headers: Vec<(String, String)>,
    pub declared: Option<usize>,
    pub slow: bool,
}
impl Response {
    pub fn ok(body: &[u8]) -> Self {
        Self {
            status: 200,
            body: body.to_vec(),
            headers: vec![
                ("Content-Type".into(), "video/mp4".into()),
                ("ETag".into(), "\"v1\"".into()),
            ],
            declared: None,
            slow: false,
        }
    }
}
pub struct Server {
    pub base: String,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}
impl Server {
    pub fn new(handler: impl Fn(Request) -> Response + Send + Sync + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let done = stop.clone();
        let handler = Arc::new(handler);
        let thread = thread::spawn(move || {
            while !done.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let handler = handler.clone();
                        thread::spawn(move || serve(stream, &*handler));
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2))
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            base: format!("http://{address}"),
            stop,
            thread: Some(thread),
        }
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
    }
}
fn serve(mut stream: TcpStream, handler: &(dyn Fn(Request) -> Response + Send + Sync)) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(3)));
    let mut bytes = Vec::new();
    let mut buffer = [0u8; 4096];
    while !bytes.windows(4).any(|s| s == b"\r\n\r\n") {
        let Ok(n) = stream.read(&mut buffer) else {
            return;
        };
        if n == 0 {
            return;
        }
        bytes.extend_from_slice(&buffer[..n]);
        if bytes.len() > 65536 {
            return;
        }
    }
    let text = String::from_utf8_lossy(&bytes);
    let mut lines = text.split("\r\n");
    let first = lines.next().unwrap();
    let method = first.split_whitespace().next().unwrap().to_owned();
    let path = first.split_whitespace().nth(1).unwrap().to_owned();
    let headers = lines
        .filter_map(|s| {
            s.split_once(':')
                .map(|(a, b)| (a.to_ascii_lowercase(), b.trim().to_owned()))
        })
        .collect();
    let response = handler(Request {
        method,
        path,
        headers,
    });
    let chunked = response
        .headers
        .iter()
        .any(|(k, v)| k.eq_ignore_ascii_case("transfer-encoding") && v == "chunked");
    let mut head = format!(
        "HTTP/1.1 {} Result\r\nConnection: close\r\n",
        response.status
    );
    if !chunked {
        head.push_str(&format!(
            "Content-Length: {}\r\n",
            response.declared.unwrap_or(response.body.len())
        ));
    }
    for (k, v) in response.headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    if stream.write_all(head.as_bytes()).is_err() {
        return;
    }
    for chunk in response.body.chunks(1024) {
        if chunked && write!(stream, "{:x}\r\n", chunk.len()).is_err() {
            return;
        }
        if stream.write_all(chunk).is_err() {
            return;
        }
        if chunked && stream.write_all(b"\r\n").is_err() {
            return;
        }
        if response.slow {
            thread::sleep(Duration::from_millis(5));
        }
    }
    if chunked {
        let _ = stream.write_all(b"0\r\n\r\n");
    }
}
