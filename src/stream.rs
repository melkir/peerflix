//! Serves one file over HTTP with range support, which is what players need
//! to start playback early and to seek.

use std::{convert::Infallible, io::SeekFrom, path::Path, pin::Pin, sync::Arc, time::Duration};

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
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncSeek, AsyncSeekExt},
    net::TcpListener,
};
use tokio_util::io::ReaderStream;

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

/// Serves file on every path until the task is dropped.
pub async fn serve(listener: TcpListener, file: Arc<File>) {
    loop {
        let sock = match listener.accept().await {
            Ok((sock, _)) => sock,
            Err(_) => {
                // Usually running out of file descriptors; give it a moment.
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let file = file.clone();
        tokio::spawn(async move {
            let svc = service_fn(move |req| {
                let file = file.clone();
                async move { Ok::<_, Infallible>(handle(&file, &req).await) }
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
        return Response::builder()
            .status(StatusCode::METHOD_NOT_ALLOWED)
            .header(header::ALLOW, "GET, HEAD")
            .body(empty())
            .unwrap();
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

fn empty() -> Body {
    Empty::new().map_err(|e| match e {}).boxed_unsync()
}

fn error(e: anyhow::Error) -> Response<Body> {
    let mut resp = Response::new(
        Full::new(Bytes::from(format!("{e:#}\n")))
            .map_err(|e| match e {})
            .boxed_unsync(),
    );
    *resp.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    resp
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
            Range::Partial(start, (end + 1).min(len)).nonempty(len)
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

fn content_type(name: &str) -> &'static str {
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
    fn escapes_paths() {
        assert_eq!(
            path_escape("[Grp] Show - 01 (1080p).mkv"),
            "%5BGrp%5D%20Show%20-%2001%20%281080p%29.mkv"
        );
        assert_eq!(path_escape("a/b?é"), "a%2Fb%3F%C3%A9");
    }

    async fn serve_bytes(data: &'static [u8]) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/video.mkv", listener.local_addr().unwrap());
        let file = File {
            name: "video.mkv".into(),
            len: data.len() as u64,
            open: Box::new(move || {
                Box::pin(async move { Ok(Box::pin(std::io::Cursor::new(data)) as Reader) })
            }),
        };
        tokio::spawn(serve(listener, Arc::new(file)));
        url
    }

    #[tokio::test]
    async fn serves_ranges() {
        let data: &'static [u8] = b"0123456789".repeat(5000).leak();
        let url = serve_bytes(data).await;
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
    }
}
