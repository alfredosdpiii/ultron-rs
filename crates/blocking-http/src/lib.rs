//! One blocking HTTP exchange, for callers on host request threads (the Hindsight client, the judge seat): a request,
//! its status, its `retry-after` header and (for a 2xx answer) its body read up to a size limit, cancelled by a signal
//! or a deadline. [`ReqwestTransport`] is the real one.

use kernel::Signal;
use std::sync::mpsc;
use std::time::Duration;

/// One request as the client builds it. `headers` are final (names lower-case, values validated).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
}

/// A status, its `retry-after` header (as sent, any status) and, for a 2xx status only, the whole body (empty when
/// there is none). A non-2xx body is never read.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HttpResponse {
    pub status: u16,
    pub retry_after: Option<String>,
    pub body: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportError {
    /// The deadline passed before the body was read.
    Timeout,
    /// The signal was aborted before the body was read.
    Aborted,
    /// A 2xx answer declared, or streamed, more than the limit.
    TooLarge,
    /// No answer: connection, protocol or redirect (redirects are refused, not followed).
    Failed,
}

/// Sends one request. A 2xx answer whose `content-length` (read as a JavaScript `Number`) exceeds `max_bytes`, or
/// whose body grows past it, is [`TransportError::TooLarge`]; the deadline (`timeout` from the call) and the signal
/// cover the whole exchange, body included.
pub trait Transport: Send + Sync {
    fn send(
        &self,
        request: HttpRequest,
        max_bytes: usize,
        timeout: Duration,
        signal: &Signal,
    ) -> Result<HttpResponse, TransportError>;
}

/// reqwest with rustls on a private one-thread runtime, so blocking callers (host request threads) never run inside
/// another runtime. No proxy (as Node's fetch), no redirects, no decompression.
pub struct ReqwestTransport {
    client: reqwest::Client,
    runtime: Option<tokio::runtime::Runtime>,
}

impl ReqwestTransport {
    pub fn new() -> Result<Self, String> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name("blocking-http")
            .enable_all()
            .build()
            .map_err(|e| e.to_string())?;
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Self {
            client,
            runtime: Some(runtime),
        })
    }
}

impl Drop for ReqwestTransport {
    fn drop(&mut self) {
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }
    }
}

/// `Number(length) > max` for a `content-length` value; the HTTP stack has already rejected malformed ones.
fn declared_over(length: &str, max: usize) -> bool {
    length.trim().parse::<f64>().is_ok_and(|n| n > max as f64)
}

fn build_request(client: &reqwest::Client, request: HttpRequest) -> Result<reqwest::RequestBuilder, TransportError> {
    let method = reqwest::Method::from_bytes(request.method.as_bytes()).map_err(|_| TransportError::Failed)?;
    let mut builder = client.request(method, &request.url);
    for (name, value) in &request.headers {
        let bytes: Vec<u8> = value.chars().map(|c| c as u32 as u8).collect();
        let value = reqwest::header::HeaderValue::from_bytes(&bytes).map_err(|_| TransportError::Failed)?;
        builder = builder.header(name.as_str(), value);
    }
    if let Some(body) = request.body {
        builder = builder.body(body);
    }
    Ok(builder)
}

async fn read_body_limited(response: &mut reqwest::Response, max_bytes: usize) -> Result<Vec<u8>, TransportError> {
    if let Some(length) = response.headers().get(reqwest::header::CONTENT_LENGTH)
        && declared_over(&String::from_utf8_lossy(length.as_bytes()), max_bytes)
    {
        return Err(TransportError::TooLarge);
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| TransportError::Failed)? {
        if body.len() + chunk.len() > max_bytes {
            return Err(TransportError::TooLarge);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

async fn exchange(
    client: reqwest::Client,
    request: HttpRequest,
    max_bytes: usize,
) -> Result<HttpResponse, TransportError> {
    let builder = build_request(&client, request)?;
    let mut response = builder.send().await.map_err(|_| TransportError::Failed)?;
    let status = response.status().as_u16();
    let retry_after = response
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .map(|value| String::from_utf8_lossy(value.as_bytes()).into_owned());
    if !(200..300).contains(&status) {
        return Ok(HttpResponse {
            status,
            retry_after,
            body: Vec::new(),
        });
    }
    let body = read_body_limited(&mut response, max_bytes).await?;
    Ok(HttpResponse {
        status,
        retry_after,
        body,
    })
}

impl Transport for ReqwestTransport {
    fn send(
        &self,
        request: HttpRequest,
        max_bytes: usize,
        timeout: Duration,
        signal: &Signal,
    ) -> Result<HttpResponse, TransportError> {
        let Some(runtime) = &self.runtime else {
            return Err(TransportError::Failed);
        };
        if signal.is_aborted() {
            return Err(TransportError::Aborted);
        }
        let (done, outcome) = mpsc::channel();
        let task = runtime.spawn({
            let client = self.client.clone();
            async move {
                let _ = done.send(exchange(client, request, max_bytes).await);
            }
        });
        let abort = task.abort_handle();
        let _link = signal.link(move |_| abort.abort());
        let result = match outcome.recv_timeout(timeout) {
            Ok(result) => result,
            Err(mpsc::RecvTimeoutError::Timeout) => Err(TransportError::Timeout),
            Err(mpsc::RecvTimeoutError::Disconnected) if signal.is_aborted() => Err(TransportError::Aborted),
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(TransportError::Failed),
        };
        task.abort();
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Condvar, Mutex};

    fn spawn_server(
        handler: impl FnOnce(std::net::TcpStream) + Send + 'static,
    ) -> (String, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let addr = listener.local_addr().expect("local addr");
        let handle = std::thread::spawn(move || {
            if let Ok((stream, _)) = listener.accept() {
                handler(stream);
            }
        });
        (format!("http://{addr}"), handle)
    }

    fn read_http_head(stream: &mut std::net::TcpStream) -> String {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 256];
        while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
            let n = stream.read(&mut chunk).unwrap_or(0);
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
        }
        String::from_utf8_lossy(&buf).into_owned()
    }

    #[test]
    fn round_trips_status_headers_and_body() {
        let transport = ReqwestTransport::new().expect("transport");
        let (url, handle) = spawn_server(|mut stream| {
            let head = read_http_head(&mut stream);
            assert!(head.starts_with("POST /echo HTTP/1.1"), "{head}");
            assert!(head.to_ascii_lowercase().contains("x-test: ok"), "{head}");

            let body = b"hello world";
            let reply = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nRetry-After: 3\r\n\r\n",
                body.len()
            );
            stream.write_all(reply.as_bytes()).unwrap();
            stream.write_all(body).unwrap();
        });

        let req = HttpRequest {
            method: "POST".into(),
            url: format!("{url}/echo"),
            headers: vec![("x-test".into(), "ok".into())],
            body: Some(b"ping".to_vec()),
        };
        let resp = transport
            .send(req, 64, Duration::from_secs(5), &Signal::new())
            .expect("200 ok");
        assert_eq!(resp.status, 200);
        assert_eq!(resp.retry_after.as_deref(), Some("3"));
        assert_eq!(resp.body, b"hello world");
        handle.join().unwrap();
    }

    #[test]
    fn skips_body_on_non_2xx_and_redirects() {
        let transport = ReqwestTransport::new().expect("transport");
        let (url, handle) = spawn_server(|mut stream| {
            assert!(!read_http_head(&mut stream).is_empty());
            let reply = concat!(
                "HTTP/1.1 429 Too Many Requests\r\n",
                "Retry-After: 12\r\n",
                "Content-Length: 9\r\n\r\n",
                "slow down",
            );
            stream.write_all(reply.as_bytes()).unwrap();
        });
        let resp = transport
            .send(
                HttpRequest {
                    method: "GET".into(),
                    url,
                    headers: Vec::new(),
                    body: None,
                },
                64,
                Duration::from_secs(5),
                &Signal::new(),
            )
            .expect("429 response");
        assert_eq!(resp.status, 429);
        assert_eq!(resp.retry_after.as_deref(), Some("12"));
        assert!(resp.body.is_empty());
        handle.join().unwrap();

        let (redir_url, redir_handle) = spawn_server(|mut stream| {
            assert!(!read_http_head(&mut stream).is_empty());
            let reply = concat!(
                "HTTP/1.1 302 Found\r\n",
                "Location: http://127.0.0.1:1/never\r\n",
                "Content-Length: 0\r\n\r\n",
            );
            stream.write_all(reply.as_bytes()).unwrap();
        });
        let redir = transport
            .send(
                HttpRequest {
                    method: "GET".into(),
                    url: redir_url,
                    headers: Vec::new(),
                    body: None,
                },
                64,
                Duration::from_secs(5),
                &Signal::new(),
            )
            .expect("302 not followed");
        assert_eq!(redir.status, 302);
        assert!(redir.body.is_empty());
        redir_handle.join().unwrap();
    }

    #[test]
    fn enforces_max_body_bytes() {
        let transport = ReqwestTransport::new().expect("transport");
        let (url_declared, h1) = spawn_server(|mut stream| {
            assert!(!read_http_head(&mut stream).is_empty());
            let reply = "HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n";
            drop(stream.write_all(reply.as_bytes()));
        });
        let err = transport
            .send(
                HttpRequest {
                    method: "GET".into(),
                    url: url_declared,
                    headers: Vec::new(),
                    body: None,
                },
                16,
                Duration::from_secs(5),
                &Signal::new(),
            )
            .unwrap_err();
        assert_eq!(err, TransportError::TooLarge);
        h1.join().unwrap();

        let (url_chunked, h2) = spawn_server(|mut stream| {
            assert!(!read_http_head(&mut stream).is_empty());
            let reply = concat!(
                "HTTP/1.1 200 OK\r\n",
                "Transfer-Encoding: chunked\r\n\r\n",
                "14\r\n01234567890123456789\r\n0\r\n\r\n",
            );
            drop(stream.write_all(reply.as_bytes()));
        });
        let err = transport
            .send(
                HttpRequest {
                    method: "GET".into(),
                    url: url_chunked,
                    headers: Vec::new(),
                    body: None,
                },
                16,
                Duration::from_secs(5),
                &Signal::new(),
            )
            .unwrap_err();
        assert_eq!(err, TransportError::TooLarge);
        h2.join().unwrap();
    }

    #[test]
    fn cancels_times_out_and_rejects_bad_method() {
        let transport = ReqwestTransport::new().expect("transport");

        let pre_aborted = Signal::new();
        pre_aborted.abort("cancelled");
        let err = transport
            .send(
                HttpRequest {
                    method: "GET".into(),
                    url: "http://127.0.0.1:1".into(),
                    headers: Vec::new(),
                    body: None,
                },
                64,
                Duration::from_secs(5),
                &pre_aborted,
            )
            .unwrap_err();
        assert_eq!(err, TransportError::Aborted);

        let err = transport
            .send(
                HttpRequest {
                    method: "BAD METHOD".into(),
                    url: "http://127.0.0.1:1".into(),
                    headers: Vec::new(),
                    body: None,
                },
                64,
                Duration::from_secs(5),
                &Signal::new(),
            )
            .unwrap_err();
        assert_eq!(err, TransportError::Failed);

        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let server_gate = gate.clone();
        let (url_abort, h_abort) = spawn_server(move |mut stream| {
            assert!(!read_http_head(&mut stream).is_empty());
            let (lock, cvar) = &*server_gate;
            let mut released = lock.lock().unwrap();
            *released = true;
            cvar.notify_all();
            drop(cvar.wait_timeout_while(released, Duration::from_secs(5), |done| *done));
        });
        let signal = Signal::new();
        let aborter_signal = signal.clone();
        let aborter_gate = gate.clone();
        let aborter = std::thread::spawn(move || {
            let (lock, cvar) = &*aborter_gate;
            let guard = lock.lock().unwrap();
            drop(cvar.wait_while(guard, |seen| !*seen));
            aborter_signal.abort("stop");
        });
        let err = transport
            .send(
                HttpRequest {
                    method: "GET".into(),
                    url: url_abort,
                    headers: Vec::new(),
                    body: None,
                },
                64,
                Duration::from_secs(5),
                &signal,
            )
            .unwrap_err();
        assert_eq!(err, TransportError::Aborted);
        {
            let (lock, cvar) = &*gate;
            *lock.lock().unwrap() = false;
            cvar.notify_all();
        }
        aborter.join().unwrap();
        h_abort.join().unwrap();

        let timeout_gate = Arc::new((Mutex::new(false), Condvar::new()));
        let server_timeout_gate = timeout_gate.clone();
        let (url_timeout, h_timeout) = spawn_server(move |mut stream| {
            assert!(!read_http_head(&mut stream).is_empty());
            let (lock, cvar) = &*server_timeout_gate;
            let guard = lock.lock().unwrap();
            drop(cvar.wait_timeout_while(guard, Duration::from_secs(5), |done| !*done));
        });
        let err = transport
            .send(
                HttpRequest {
                    method: "GET".into(),
                    url: url_timeout,
                    headers: Vec::new(),
                    body: None,
                },
                64,
                Duration::from_millis(50),
                &Signal::new(),
            )
            .unwrap_err();
        assert_eq!(err, TransportError::Timeout);
        {
            let (lock, cvar) = &*timeout_gate;
            *lock.lock().unwrap() = true;
            cvar.notify_all();
        }
        h_timeout.join().unwrap();
    }
}
