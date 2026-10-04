//! Serving files over HTTP with range support, which is what players need to
//! start playback early and to seek, and a control to start, follow, pause
//! and stop the stream.

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

use anyhow::Context;
use bytes::Bytes;
use futures_util::{TryStreamExt, future::BoxFuture};
use http_body_util::{BodyExt, Empty, Full, StreamBody, combinators::UnsyncBoxBody};
use hyper::{
    Method, Request, Response, StatusCode,
    body::Frame,
    header::{self, HeaderName, HeaderValue},
    server::conn::http1,
    service::service_fn,
};
use hyper_util::rt::TokioIo;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncSeek, AsyncSeekExt},
    net::TcpListener,
    sync::{mpsc, oneshot},
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
    pub reply: Reply,
}

/// Where to send a pick's answer: the stream's JSON, or why it didn't start.
pub type Reply = oneshot::Sender<Result<String, String>>;

/// The download behind a stream, which its control reports on and pauses.
pub trait Download: Send + Sync {
    /// How it's going, as JSON.
    fn status_json(&self) -> String;

    /// Pauses it, with paused, or resumes it.
    fn set_paused(&self, paused: bool) -> BoxFuture<'static, anyhow::Result<()>>;
}

/// The port streams are served on unless another is asked for.
pub const DEFAULT_PORT: u16 = 8888;

/// Binds a stream's port on localhost. Without an explicit port, that's
/// DEFAULT_PORT, or a random one when it's taken.
pub async fn bind_listener(port: Option<u16>) -> anyhow::Result<TcpListener> {
    let want = port.unwrap_or(DEFAULT_PORT);
    match TcpListener::bind(("127.0.0.1", want)).await {
        Ok(l) => Ok(l),
        Err(e) if port.is_none() && e.kind() == std::io::ErrorKind::AddrInUse => {
            Ok(TcpListener::bind(("127.0.0.1", 0)).await?)
        }
        Err(e) => Err(e).with_context(|| format!("listening on port {want}")),
    }
}

/// An HTTP server on localhost for a stream, until dropped.
///
/// At CONTROL_PATH, it answers for the stream itself: a PUT asks to start
/// streaming, with `?index=N` for a given file, and once it has started,
/// `?pause` and `?resume` pause and resume its download; a GET returns the
/// stream's status once it has started, and a DELETE cancels the server's
/// stop token.
/// Once streaming, it serves each file at `/<escaped name>`, however its
/// escapes are written, and the first at `/`.
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

/// The files a server streams, and their download.
struct Streaming {
    files: Vec<File>,
    download: Arc<dyn Download>,
}

impl Streaming {
    /// The file at path, the escaped name of one, or the first at the root.
    fn file(&self, path: &str) -> Option<&File> {
        if path.is_empty() {
            return self.files.first();
        }
        let name = path_unescape(path)?;
        self.files.iter().find(|f| f.name.as_bytes() == name)
    }
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

    /// Starts streaming files, which the control reports on and pauses as
    /// download. A server streams once.
    pub fn stream(&self, files: Vec<File>, download: Arc<dyn Download>) {
        let streaming = Streaming { files, download };
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
                Some(streaming) => json(streaming.download.status_json()),
                None => text(StatusCode::CONFLICT, "not streaming yet"),
            },
            // A web page can't send PUT or DELETE to another site without
            // asking first in a CORS preflight, which gets a 405 below.
            Method::PUT => match put_query(query) {
                Some(Put::Pick(index)) => self.pick(index).await,
                Some(Put::Pause(paused)) => self.pause(paused).await,
                None => text(StatusCode::BAD_REQUEST, "index isn't a number"),
            },
            Method::DELETE => {
                self.stop.cancel();
                no_content()
            }
            _ => not_allowed("GET, PUT, DELETE"),
        }
    }

    /// Pauses the download, or resumes it, once streaming.
    async fn pause(&self, paused: bool) -> Response<Body> {
        let Some(streaming) = self.streaming.get() else {
            return text(StatusCode::CONFLICT, "not streaming yet");
        };
        match streaming.download.set_paused(paused).await {
            Ok(()) => no_content(),
            // Such as pausing it twice.
            Err(e) => text(StatusCode::CONFLICT, format!("{e:#}")),
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

/// What a PUT to the control asks for.
#[derive(Debug, PartialEq)]
enum Put {
    /// To start streaming the file at the index, if there's one.
    Pick(Option<usize>),
    /// To pause the download, with true, or resume it.
    Pause(bool),
}

/// Parses a PUT's query: `pause` or `resume`, or the file to pick, as in
/// `index=3`, if there's one. None if the index isn't a number.
fn put_query(query: Option<&str>) -> Option<Put> {
    let mut pairs = query.unwrap_or("").split('&');
    if let Some(pair) = pairs.clone().find(|&p| p == "pause" || p == "resume") {
        return Some(Put::Pause(pair == "pause"));
    }
    let value = pairs.find_map(|pair| pair.strip_prefix("index="));
    let index = value.map(str::parse).transpose().ok()?;
    Some(Put::Pick(index))
}

/// How long a stream goes without a player connected before it ends: the
/// player may have gone away, or lingers without its window. IINA keeps its
/// connection while the video is open, paused or not.
pub const IDLE: Duration = Duration::from_secs(30);

/// Counts the connections players have open to a server, to tell whether
/// one is still watching. A connection counts once it asks for a file, so one
/// that only asks for the control, such as a plugin showing the status, doesn't.
#[derive(Clone, Debug, Default)]
pub struct Connections(Arc<AtomicUsize>);

impl Connections {
    pub fn count(&self) -> usize {
        self.0.load(Ordering::Relaxed)
    }

    /// Returns once no player has been connected for IDLE, with the time
    /// before the first one connects counting too.
    pub async fn idle(&self) {
        let mut ticker = tokio::time::interval(Duration::from_secs(1));
        let mut since = tokio::time::Instant::now();
        loop {
            ticker.tick().await;
            if self.count() > 0 {
                since = tokio::time::Instant::now();
            } else if since.elapsed() >= IDLE {
                return;
            }
        }
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
        let Some(file) = streaming.file(path) else {
            return text(StatusCode::NOT_FOUND, "no such file");
        };
        self.player.get_or_init(|| shared.connections.open());
        handle(file, &req).await
    }
}

/// Accepts connections to a server until the task is dropped.
async fn accept(listener: TcpListener, shared: Arc<Shared>) {
    loop {
        let Ok((sock, _)) = listener.accept().await else {
            // Usually running out of file descriptors; give it a moment.
            tokio::time::sleep(Duration::from_millis(100)).await;
            continue;
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
            let range = content_range(format_args!("*/{}", file.len));
            return response(
                StatusCode::RANGE_NOT_SATISFIABLE,
                [(header::CONTENT_RANGE, range)],
                empty(),
            );
        }
    };

    let mut resp = response(
        status,
        [
            (header::ACCEPT_RANGES, HeaderValue::from_static("bytes")),
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static(content_type(&file.name)),
            ),
            (header::CONTENT_LENGTH, HeaderValue::from(end - start)),
        ],
        empty(),
    );
    if status == StatusCode::PARTIAL_CONTENT {
        let range = content_range(format_args!("{start}-{}/{}", end - 1, file.len));
        resp.headers_mut().insert(header::CONTENT_RANGE, range);
    }
    if req.method() == Method::HEAD {
        return resp;
    }

    let reader = match (file.open)().await {
        Ok(mut r) => match r.seek(SeekFrom::Start(start)).await {
            Ok(_) => r,
            Err(e) => return error(&e.into()),
        },
        Err(e) => return error(&e),
    };
    let body = StreamBody::new(
        ReaderStream::with_capacity(reader.take(end - start), 64 << 10).map_ok(Frame::data),
    );
    *resp.body_mut() = BodyExt::boxed_unsync(body);
    resp
}

/// A Content-Range header of bytes range, such as `0-9/100`.
fn content_range(range: std::fmt::Arguments<'_>) -> HeaderValue {
    HeaderValue::try_from(format!("bytes {range}")).expect("byte ranges are valid header values")
}

/// Whether a Host header names the loopback address the server listens on,
/// or is absent, as with HTTP/1.0 clients.
fn local_host(host: Option<&HeaderValue>) -> bool {
    let Some(host) = host else {
        return true;
    };
    let Ok(host) = host.to_str() else {
        return false;
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

/// Builds a response directly rather than with `Response::builder`, whose
/// header conversions can fail, which these can't.
fn response<const N: usize>(
    status: StatusCode,
    headers: [(HeaderName, HeaderValue); N],
    body: Body,
) -> Response<Body> {
    let mut resp = Response::new(body);
    *resp.status_mut() = status;
    resp.headers_mut().extend(headers);
    resp
}

fn no_content() -> Response<Body> {
    response(StatusCode::NO_CONTENT, [], empty())
}

fn not_allowed(allow: &'static str) -> Response<Body> {
    response(
        StatusCode::METHOD_NOT_ALLOWED,
        [(header::ALLOW, HeaderValue::from_static(allow))],
        empty(),
    )
}

fn json(body: String) -> Response<Body> {
    response(
        StatusCode::OK,
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            ),
            (header::CACHE_CONTROL, HeaderValue::from_static("no-store")),
        ],
        full(body),
    )
}

fn text(status: StatusCode, body: impl Into<String>) -> Response<Body> {
    response(
        status,
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/plain; charset=utf-8"),
        )],
        full(body.into()),
    )
}

fn error(e: &anyhow::Error) -> Response<Body> {
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
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(name.len());
    for b in name.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(char::from(b));
        } else {
            out.push('%');
            out.push(char::from(HEX[usize::from(b >> 4)]));
            out.push(char::from(HEX[usize::from(b & 0xF)]));
        }
    }
    out
}

/// Undoes path_escape, or any other percent-encoding of a path segment,
/// returning the bytes escaped; None if an escape is malformed.
fn path_unescape(path: &str) -> Option<Vec<u8>> {
    let hex = |b: u8| char::from(b).to_digit(16);
    let mut out = Vec::with_capacity(path.len());
    let mut bytes = path.bytes();
    while let Some(b) = bytes.next() {
        if b == b'%' {
            let (hi, lo) = (hex(bytes.next()?)?, hex(bytes.next()?)?);
            out.push(u8::try_from(hi << 4 | lo).ok()?);
        } else {
            out.push(b);
        }
    }
    Some(out)
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
        for name in ["[Grp] Show - 01 (1080p).mkv", "a/b?é", ""] {
            assert_eq!(path_unescape(&path_escape(name)).unwrap(), name.as_bytes());
        }
        // However a client writes the escapes.
        assert_eq!(
            path_unescape("%5bGrp%5d (1080p)").unwrap(),
            b"[Grp] (1080p)"
        );
        for bad in ["%", "%5", "%zz"] {
            assert_eq!(path_unescape(bad), None, "{bad}");
        }
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

    /// A download with a fixed status, which records its pauses, failing a
    /// second one in a row as librqbit does.
    #[derive(Default)]
    struct FakeDownload {
        status: &'static str,
        paused: Arc<std::sync::Mutex<Vec<bool>>>,
    }

    impl Download for FakeDownload {
        fn status_json(&self) -> String {
            self.status.into()
        }

        fn set_paused(&self, p: bool) -> BoxFuture<'static, anyhow::Result<()>> {
            let paused = self.paused.clone();
            Box::pin(async move {
                let mut paused = paused.lock().unwrap();
                if paused.last() == Some(&p) {
                    anyhow::bail!("already {}", if p { "paused" } else { "running" });
                }
                paused.push(p);
                Ok(())
            })
        }
    }

    fn download(status: &'static str) -> Arc<FakeDownload> {
        Arc::new(FakeDownload {
            status,
            ..Default::default()
        })
    }

    /// Starts a server streaming files, with status for its control.
    async fn streaming(files: Vec<File>, status: &'static str) -> Server {
        let (server, _) = start(CancellationToken::new()).await;
        server.stream(files, download(status));
        server
    }

    #[tokio::test]
    async fn serves_files_by_name() {
        let files = vec![
            bytes_file("video.mkv", b"video"),
            bytes_file("Show E01.en.srt", b"subs"),
        ];
        let server = streaming(files, "{}").await;
        let base = server.url("");
        for (path, want) in [
            ("video.mkv", "video"),
            ("Show%20E01.en.srt", "subs"),
            ("Show E01.en.srt", "subs"),
            ("", "video"),
        ] {
            let body = reqwest::get(format!("{base}{path}")).await.unwrap();
            assert_eq!(body.text().await.unwrap(), want, "{path}");
        }
        // Rather than the video, which a player asking for subtitles would
        // take for them.
        for path in ["anything", "Show%2"] {
            let resp = reqwest::get(format!("{base}{path}")).await.unwrap();
            assert_eq!(resp.status(), 404, "{path}");
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
        server.stream(vec![bytes_file("video.mkv", b"video")], download("{}"));
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
        assert_eq!(
            reqwest::Client::new()
                .put(format!("{}?pause", server.control_url()))
                .send()
                .await
                .unwrap()
                .status(),
            409,
            "pausing before streaming"
        );
        let fake = download(r#"{"downloaded":5}"#);
        let paused = fake.paused.clone();
        server.stream(vec![bytes_file("video.mkv", b"video")], fake);
        let url = server.control_url();
        let client = reqwest::Client::new();

        for (query, want) in [("pause", 204), ("pause", 409), ("resume", 204)] {
            let resp = client.put(format!("{url}?{query}")).send().await.unwrap();
            assert_eq!(resp.status(), want, "{query}");
        }
        assert_eq!(*paused.lock().unwrap(), [true, false]);

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
    fn put_queries() {
        for (query, want) in [
            (None, Some(Put::Pick(None))),
            (Some(""), Some(Put::Pick(None))),
            (Some("index=3"), Some(Put::Pick(Some(3)))),
            (Some("x=1&index=12"), Some(Put::Pick(Some(12)))),
            (Some("index="), None),
            (Some("index=-1"), None),
            (Some("pause"), Some(Put::Pause(true))),
            (Some("x=1&resume"), Some(Put::Pause(false))),
        ] {
            assert_eq!(put_query(query), want, "{query:?}");
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
