//! Serves files over HTTP with range support, which is what players need to
//! start playback early and to seek.

use std::{
    convert::Infallible,
    io::SeekFrom,
    path::Path,
    pin::Pin,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use bytes::Bytes;
use futures_util::{TryStreamExt, future::BoxFuture};
use http_body_util::{BodyExt, Empty, Full, StreamBody, combinators::UnsyncBoxBody};
use hyper::{
    Method, Request, Response, StatusCode,
    body::Frame,
    header::{self, HeaderValue},
    server::conn::http1,
    service::service_fn,
};
use hyper_util::rt::TokioIo;
use tokio::sync::{mpsc, oneshot};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncSeek, AsyncSeekExt},
    net::TcpListener,
};
use tokio_util::{io::ReaderStream, sync::CancellationToken, task::AbortOnDropHandle};

pub trait ReadSeek: AsyncRead + AsyncSeek + Send {}
impl<T: AsyncRead + AsyncSeek + Send> ReadSeek for T {}

pub type Reader = Pin<Box<dyn ReadSeek>>;

pub struct File {
    pub name: String,
    pub len: u64,
    /// Opens a new reader for each request. With a torrent, each reader
    /// prioritizes the pieces around its own position.
    pub open: Box<dyn Fn() -> BoxFuture<'static, anyhow::Result<Reader>> + Send + Sync>,
}

type Body = UnsyncBoxBody<Bytes, std::io::Error>;

/// Where the server answers for the stream itself rather than a file. File
/// names are served escaped, so none can take a path with a slash.
pub const CONTROL_PATH: &str = "peerflix/stream";

/// A request to start streaming, from a PUT to CONTROL_PATH: the index of
/// the file it asked for, if any, and where to send the JSON to answer with,
/// or why streaming didn't start.
pub struct Pick {
    pub index: Option<usize>,
    pub reply: oneshot::Sender<Result<String, String>>,
}

/// An HTTP server on localhost for a stream, until dropped.
///
/// At CONTROL_PATH, it answers for the stream itself: a PUT asks to start
/// streaming, with `?index=N` for a given file, a GET returns the stream's
/// status once it has started, and a DELETE cancels the server's stop token.
/// Once streaming, it serves each file at `/<escaped name>`, and the first on
/// every other path.
pub struct Server {
    base: String,
    shared: Arc<Shared>,
    _task: AbortOnDropHandle<()>,
}

/// What a server's connections share.
struct Shared {
    streaming: OnceLock<Streaming>,
    picks: mpsc::Sender<Pick>,
    stop: CancellationToken,
    connections: Connections,
}

/// The files a server streams, and how to tell their download's status.
struct Streaming {
    files: Vec<File>,
    /// The escaped names of files, in the same order.
    paths: Vec<String>,
    status: Box<dyn Fn() -> String + Send + Sync>,
}

impl Server {
    /// Starts serving on listener, cancelling stop on a DELETE. The picks PUT
    /// to the control come out of the returned receiver; dropping it turns
    /// them down.
    pub fn start(
        listener: TcpListener,
        stop: CancellationToken,
    ) -> std::io::Result<(Server, mpsc::Receiver<Pick>)> {
        let base = format!("http://{}", listener.local_addr()?);
        let (picks, received) = mpsc::channel(1);
        let shared = Arc::new(Shared {
            streaming: OnceLock::new(),
            picks,
            stop,
            connections: Connections::default(),
        });
        let task = AbortOnDropHandle::new(tokio::spawn(accept(listener, shared.clone())));
        let server = Server {
            base,
            shared,
            _task: task,
        };
        Ok((server, received))
    }

    /// The URL a file named name is streamed at.
    pub fn url(&self, name: &str) -> String {
        format!("{}/{}", self.base, path_escape(name))
    }

    pub fn control_url(&self) -> String {
        format!("{}/{CONTROL_PATH}", self.base)
    }

    /// The connections players have open.
    pub fn connections(&self) -> &Connections {
        &self.shared.connections
    }

    /// Starts streaming files, with status for the control's GET. A server
    /// streams once.
    pub fn stream(&self, files: Vec<File>, status: impl Fn() -> String + Send + Sync + 'static) {
        let paths = files.iter().map(|f| path_escape(&f.name)).collect();
        let streaming = Streaming {
            files,
            paths,
            status: Box::new(status),
        };
        let first = self.shared.streaming.set(streaming).is_ok();
        assert!(first, "a server streams once");
    }
}

impl Shared {
    async fn control(&self, method: &Method, query: Option<&str>) -> Response<Body> {
        match *method {
            // Without CORS headers, a web page can send this but can't read
            // the answer.
            Method::GET => match self.streaming.get() {
                Some(streaming) => json((streaming.status)()),
                None => text(StatusCode::CONFLICT, "not streaming yet"),
            },
            // A web page can't send PUT or DELETE to another site without
            // asking first in a CORS preflight, which gets a 405 below.
            Method::PUT => match query_index(query) {
                Ok(index) => self.pick(index).await,
                Err(()) => text(StatusCode::BAD_REQUEST, "index isn't a number"),
            },
            Method::DELETE => {
                self.stop.cancel();
                Response::builder()
                    .status(StatusCode::NO_CONTENT)
                    .body(empty())
                    .unwrap()
            }
            _ => not_allowed("GET, PUT, DELETE"),
        }
    }

    /// Hands a pick of file index to the receiver of picks, and answers with
    /// what it replies.
    async fn pick(&self, index: Option<usize>) -> Response<Body> {
        let taken = || text(StatusCode::CONFLICT, "already streaming");
        let (reply, answer) = oneshot::channel();
        if self.streaming.get().is_some() || self.picks.send(Pick { index, reply }).await.is_err() {
            return taken();
        }
        match answer.await {
            Ok(Ok(stream)) => json(stream),
            Ok(Err(e)) => text(StatusCode::INTERNAL_SERVER_ERROR, e),
            // Dropped for another pick, or as peerflix stops.
            Err(_) => taken(),
        }
    }
}

/// Parses the index in a query such as `index=3`, if there's one.
fn query_index(query: Option<&str>) -> Result<Option<usize>, ()> {
    let value = query
        .unwrap_or("")
        .split('&')
        .find_map(|pair| pair.strip_prefix("index="));
    value.map(|v| v.parse().map_err(|_| ())).transpose()
}

/// Counts the connections players have open to a server, to tell whether
/// one is still watching. A connection counts once it asks for a file, so one
/// that only asks for the control, such as a plugin showing the status, doesn't.
#[derive(Clone, Debug, Default)]
pub struct Connections(Arc<AtomicUsize>);

impl Connections {
    pub fn count(&self) -> usize {
        self.0.load(Ordering::Relaxed)
    }

    /// Counts one more connection until the returned guard is dropped.
    fn open(&self) -> Open {
        self.0.fetch_add(1, Ordering::Relaxed);
        Open(self.clone())
    }
}

/// A connection counted in Connections until dropped.
struct Open(Connections);

impl Drop for Open {
    fn drop(&mut self) {
        self.0.0.fetch_sub(1, Ordering::Relaxed);
    }
}

/// One connection to a server, counted in its Connections once it asks for a
/// file.
struct Connection {
    shared: Arc<Shared>,
    player: OnceLock<Open>,
}

impl Connection {
    async fn respond<B>(&self, req: Request<B>) -> Response<Body> {
        if !local_host(req.headers().get(header::HOST)) {
            // A web page reaching the server through a rebound DNS name.
            return text(StatusCode::FORBIDDEN, "");
        }
        let shared = &self.shared;
        let path = req.uri().path().trim_start_matches('/');
        if path == CONTROL_PATH {
            return shared.control(req.method(), req.uri().query()).await;
        }
        let Some(streaming) = shared.streaming.get() else {
            return text(StatusCode::NOT_FOUND, "not streaming yet");
        };
        self.player.get_or_init(|| shared.connections.open());
        let i = streaming.paths.iter().position(|p| p == path).unwrap_or(0);
        handle(&streaming.files[i], &req).await
    }
}

/// Accepts connections to a server until the task is dropped.
async fn accept(listener: TcpListener, shared: Arc<Shared>) {
    loop {
        let sock = match listener.accept().await {
            Ok((sock, _)) => sock,
            Err(_) => {
                // Usually running out of file descriptors; give it a moment.
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let conn = Arc::new(Connection {
            shared: shared.clone(),
            player: OnceLock::new(),
        });
        tokio::spawn(async move {
            let svc = service_fn(move |req| {
                let conn = conn.clone();
                async move { Ok::<_, Infallible>(conn.respond(req).await) }
            });
            // Errors here are players closing connections to seek.
            let _ = http1::Builder::new()
                .serve_connection(TokioIo::new(sock), svc)
                .await;
        });
    }
}

async fn handle<B>(file: &File, req: &Request<B>) -> Response<Body> {
    if req.method() != Method::GET && req.method() != Method::HEAD {
        return not_allowed("GET, HEAD");
    }
    let range = if req.headers().contains_key(header::IF_RANGE) {
        // There's no validator to match it against, so send the whole file.
        Range::Full
    } else {
        parse_range(
            req.headers()
                .get(header::RANGE)
                .and_then(|v| v.to_str().ok()),
            file.len,
        )
    };
    let (status, start, end) = match range {
        Range::Full => (StatusCode::OK, 0, file.len),
        Range::Partial(start, end) => (StatusCode::PARTIAL_CONTENT, start, end),
        Range::Unsatisfiable => {
            return Response::builder()
                .status(StatusCode::RANGE_NOT_SATISFIABLE)
                .header(header::CONTENT_RANGE, format!("bytes */{}", file.len))
                .body(empty())
                .unwrap();
        }
    };

    let mut resp = Response::builder()
        .status(status)
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CONTENT_TYPE, content_type(&file.name))
        .header(header::CONTENT_LENGTH, end - start);
    if status == StatusCode::PARTIAL_CONTENT {
        resp = resp.header(
            header::CONTENT_RANGE,
            format!("bytes {start}-{}/{}", end - 1, file.len),
        );
    }
    if req.method() == Method::HEAD {
        return resp.body(empty()).unwrap();
    }

    let reader = match (file.open)().await {
        Ok(mut r) => match r.seek(SeekFrom::Start(start)).await {
            Ok(_) => r,
            Err(e) => return error(e.into()),
        },
        Err(e) => return error(e),
    };
    let body = StreamBody::new(
        ReaderStream::with_capacity(reader.take(end - start), 64 << 10).map_ok(Frame::data),
    );
    resp.body(BodyExt::boxed_unsync(body)).unwrap()
}

/// Whether a Host header names the loopback address the server listens on,
/// or is absent, as with HTTP/1.0 clients.
fn local_host(host: Option<&HeaderValue>) -> bool {
    let Some(host) = host.and_then(|h| h.to_str().ok().or(Some("?"))) else {
        return true;
    };
    let name = match host.strip_prefix('[') {
        Some(v6) => v6.split_once(']').map_or(v6, |(a, _)| a),
        None => host.rsplit_once(':').map_or(host, |(a, _)| a),
    };
    ["127.0.0.1", "::1", "localhost"]
        .iter()
        .any(|l| name.eq_ignore_ascii_case(l))
}

fn empty() -> Body {
    Empty::new().map_err(|e| match e {}).boxed_unsync()
}

fn not_allowed(allow: &'static str) -> Response<Body> {
    Response::builder()
        .status(StatusCode::METHOD_NOT_ALLOWED)
        .header(header::ALLOW, allow)
        .body(empty())
        .unwrap()
}

fn json(body: String) -> Response<Body> {
    Response::builder()
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::CACHE_CONTROL, "no-store")
        .body(full(body))
        .unwrap()
}

fn text(status: StatusCode, body: impl Into<String>) -> Response<Body> {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(full(body.into()))
        .unwrap()
}

fn error(e: anyhow::Error) -> Response<Body> {
    text(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}\n"))
}

fn full(body: String) -> Body {
    Full::new(Bytes::from(body))
        .map_err(|e| match e {})
        .boxed_unsync()
}

#[derive(Debug, PartialEq)]
enum Range {
    Full,
    /// Start and exclusive end.
    Partial(u64, u64),
    Unsatisfiable,
}

/// Parses a Range header for a file of length len. Anything but a single
/// byte range gets the whole file, which RFC 9110 allows.
fn parse_range(header: Option<&str>, len: u64) -> Range {
    let Some(spec) = header.and_then(|h| h.trim().strip_prefix("bytes=")) else {
        return Range::Full;
    };
    let Some((first, last)) = spec.trim().split_once('-') else {
        return Range::Full;
    };
    let num = |s: &str| {
        (!s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
            .then(|| s.parse::<u64>().ok())
            .flatten()
    };
    match (num(first), num(last), first.is_empty()) {
        // bytes=-N: the last N bytes.
        (None, Some(n), true) => match n {
            0 => Range::Unsatisfiable,
            n => Range::Partial(len.saturating_sub(n), len).nonempty(len),
        },
        (Some(start), None, false) if last.is_empty() => Range::Partial(start, len).nonempty(len),
        (Some(start), Some(end), false) if start <= end => {
            Range::Partial(start, end.saturating_add(1).min(len)).nonempty(len)
        }
        _ => Range::Full,
    }
}

impl Range {
    fn nonempty(self, len: u64) -> Range {
        match self {
            Range::Partial(start, _) if start >= len => Range::Unsatisfiable,
            r => r,
        }
    }
}

pub fn content_type(name: &str) -> &'static str {
    let ext = Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "mkv" => "video/x-matroska",
        "mp4" | "m4v" => "video/mp4",
        "avi" => "video/x-msvideo",
        "mov" => "video/quicktime",
        "webm" => "video/webm",
        "wmv" => "video/x-ms-wmv",
        "flv" => "video/x-flv",
        "ts" | "m2ts" => "video/mp2t",
        "mpg" | "mpeg" => "video/mpeg",
        "mp3" => "audio/mpeg",
        "flac" => "audio/flac",
        "m4a" => "audio/mp4",
        "srt" | "ass" | "ssa" | "txt" => "text/plain; charset=utf-8",
        "vtt" => "text/vtt; charset=utf-8",
        _ => "application/octet-stream",
    }
}

/// Escapes name for use as a URL path segment.
pub fn path_escape(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for b in name.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges() {
        use Range::*;
        let cases = [
            (None, Full),
            (Some("bytes=0-9"), Partial(0, 10)),
            (Some("bytes=90-"), Partial(90, 100)),
            (Some("bytes=90-500"), Partial(90, 100)),
            (Some("bytes=5-18446744073709551615"), Partial(5, 100)),
            (Some("bytes=-10"), Partial(90, 100)),
            (Some("bytes=-500"), Partial(0, 100)),
            (Some("bytes=100-"), Unsatisfiable),
            (Some("bytes=100-200"), Unsatisfiable),
            (Some("bytes=-0"), Unsatisfiable),
            (Some("bytes=9-0"), Full),
            (Some("bytes=0-1,5-6"), Full),
            (Some("bytes=x-1"), Full),
            (Some("items=0-1"), Full),
            (Some("bytes=-"), Full),
        ];
        for (header, want) in cases {
            assert_eq!(parse_range(header, 100), want, "{header:?}");
        }
    }

    #[test]
    fn local_hosts() {
        for (host, want) in [
            (None, true),
            (Some("127.0.0.1:8888"), true),
            (Some("localhost:8888"), true),
            (Some("LocalHost"), true),
            (Some("[::1]:8888"), true),
            (Some("evil.example:8888"), false),
            (Some("evil.example"), false),
            (Some("127.0.0.1.evil.example:80"), false),
            (Some(""), false),
        ] {
            let value = host.map(|h| HeaderValue::from_str(h).unwrap());
            assert_eq!(local_host(value.as_ref()), want, "{host:?}");
        }
    }

    #[test]
    fn escapes_paths() {
        assert_eq!(
            path_escape("[Grp] Show - 01 (1080p).mkv"),
            "%5BGrp%5D%20Show%20-%2001%20%281080p%29.mkv"
        );
        assert_eq!(path_escape("a/b?é"), "a%2Fb%3F%C3%A9");
    }

    fn bytes_file(name: &str, data: &'static [u8]) -> File {
        File {
            name: name.into(),
            len: data.len() as u64,
            open: Box::new(move || {
                Box::pin(async move { Ok(Box::pin(std::io::Cursor::new(data)) as Reader) })
            }),
        }
    }

    async fn start(stop: CancellationToken) -> (Server, mpsc::Receiver<Pick>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        Server::start(listener, stop).unwrap()
    }

    /// Starts a server streaming files, with status for its control.
    async fn streaming(files: Vec<File>, status: &'static str) -> Server {
        let (server, _) = start(CancellationToken::new()).await;
        server.stream(files, || status.into());
        server
    }

    #[tokio::test]
    async fn serves_files_by_name() {
        let files = vec![
            bytes_file("video.mkv", b"video"),
            bytes_file("Show E01.en.srt", b"subs"),
        ];
        let server = streaming(files, "{}").await;
        for (name, want) in [
            ("video.mkv", "video"),
            ("Show E01.en.srt", "subs"),
            ("anything", "video"),
        ] {
            let body = reqwest::get(server.url(name)).await.unwrap();
            assert_eq!(body.text().await.unwrap(), want, "{name}");
        }
    }

    #[tokio::test]
    async fn starts_streaming_on_a_pick() {
        let (server, mut picks) = start(CancellationToken::new()).await;
        let url = server.control_url();
        let client = reqwest::Client::new();
        let status = async |req: reqwest::RequestBuilder| req.send().await.unwrap().status();
        assert_eq!(status(client.get(&url)).await, 409);
        assert_eq!(status(client.get(server.url("video.mkv"))).await, 404);
        assert_eq!(status(client.put(format!("{url}?index=x"))).await, 400);

        // Answers what the receiver of picks replies.
        let put = |query: &str| tokio::spawn(client.put(format!("{url}{query}")).send());
        let failing = put("?index=9");
        let pick = picks.recv().await.unwrap();
        assert_eq!(pick.index, Some(9));
        pick.reply.send(Err("no file 9".into())).unwrap();
        let resp = failing.await.unwrap().unwrap();
        assert_eq!(resp.status(), 500);
        assert_eq!(resp.text().await.unwrap(), "no file 9");

        let starting = put("");
        let pick = picks.recv().await.unwrap();
        assert_eq!(pick.index, None);
        server.stream(vec![bytes_file("video.mkv", b"video")], || "{}".into());
        pick.reply
            .send(Ok(r#"{"name":"video.mkv"}"#.into()))
            .unwrap();
        let resp = starting.await.unwrap().unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.text().await.unwrap(), r#"{"name":"video.mkv"}"#);

        assert_eq!(status(client.put(&url)).await, 409);
        let body = client.get(server.url("video.mkv")).send().await.unwrap();
        assert_eq!(body.text().await.unwrap(), "video");
    }

    #[tokio::test]
    async fn controls_the_stream() {
        let stop = CancellationToken::new();
        let (server, _) = start(stop.clone()).await;
        server.stream(vec![bytes_file("video.mkv", b"video")], || {
            r#"{"downloaded":5}"#.into()
        });
        let url = server.control_url();
        let client = reqwest::Client::new();

        let resp = client.get(&url).send().await.unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.headers()["content-type"], "application/json");
        assert_eq!(resp.text().await.unwrap(), r#"{"downloaded":5}"#);

        for method in [reqwest::Method::POST, reqwest::Method::OPTIONS] {
            let resp = client.request(method, &url).send().await.unwrap();
            assert_eq!(resp.status(), 405);
        }
        let resp = client
            .delete(&url)
            .header("Host", "evil.example")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 403);
        assert!(!stop.is_cancelled());

        let resp = client.delete(&url).send().await.unwrap();
        assert_eq!(resp.status(), 204);
        assert!(stop.is_cancelled());
    }

    #[test]
    fn query_indexes() {
        for (query, want) in [
            (None, Ok(None)),
            (Some(""), Ok(None)),
            (Some("index=3"), Ok(Some(3))),
            (Some("x=1&index=12"), Ok(Some(12))),
            (Some("index="), Err(())),
            (Some("index=-1"), Err(())),
        ] {
            assert_eq!(query_index(query), want, "{query:?}");
        }
    }

    #[tokio::test]
    async fn counts_connections() {
        use tokio::io::AsyncWriteExt;
        let server = streaming(vec![bytes_file("video.mkv", b"video")], "{}").await;
        let connections = server.connections();
        let addr = server.base.trim_start_matches("http://").to_owned();
        // Waits up to a second for the count to reach want.
        let settles_at = async |want| {
            for _ in 0..100 {
                if connections.count() == want {
                    return true;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            false
        };
        // Opens a connection that asks for path once and stays open.
        let ask = async |path: &str| {
            let mut sock = tokio::net::TcpStream::connect(&addr).await.unwrap();
            let req = format!("HEAD /{path} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n");
            sock.write_all(req.as_bytes()).await.unwrap();
            // Answered, so the request was seen.
            assert!(sock.read(&mut [0; 256]).await.unwrap() > 0);
            sock
        };
        let a = ask("video.mkv").await;
        let b = ask("video.mkv").await;
        let _control = ask(CONTROL_PATH).await;
        let _idle = tokio::net::TcpStream::connect(&addr).await.unwrap();
        assert!(settles_at(2).await, "{}", connections.count());
        drop(a);
        assert!(settles_at(1).await, "{}", connections.count());
        drop(b);
        assert!(settles_at(0).await, "{}", connections.count());
    }

    #[tokio::test]
    async fn serves_ranges() {
        let data: &'static [u8] = b"0123456789".repeat(5000).leak();
        let server = streaming(vec![bytes_file("video.mkv", data)], "{}").await;
        let url = server.url("video.mkv");
        let client = reqwest::Client::new();

        let resp = client.get(&url).send().await.unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.headers()["content-type"], "video/x-matroska");
        assert_eq!(resp.headers()["accept-ranges"], "bytes");
        assert_eq!(resp.bytes().await.unwrap(), data);

        let resp = client
            .get(&url)
            .header("Range", "bytes=20000-20009")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 206);
        assert_eq!(resp.headers()["content-range"], "bytes 20000-20009/50000");
        assert_eq!(resp.bytes().await.unwrap(), &data[20000..20010]);

        let resp = client
            .head(&url)
            .header("Range", "bytes=-10")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 206);
        assert_eq!(resp.headers()["content-length"], "10");

        let resp = client
            .get(&url)
            .header("Range", "bytes=50000-")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 416);
        assert_eq!(resp.headers()["content-range"], "bytes */50000");

        let resp = client.post(&url).send().await.unwrap();
        assert_eq!(resp.status(), 405);

        let resp = client
            .get(&url)
            .header("Host", "evil.example")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 403);
    }
}
