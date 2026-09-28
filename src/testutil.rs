use std::{
    collections::HashMap,
    convert::Infallible,
    sync::{Arc, Mutex},
};

use bytes::Bytes;
use http_body_util::Full;
use hyper::{Response, server::conn::http1, service::service_fn};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;

/// An HTTP server that answers every request with the same status and body,
/// and records each request's query.
pub struct FakeServer {
    pub url: String,
    queries: Arc<Mutex<Vec<HashMap<String, String>>>>,
}

impl FakeServer {
    pub async fn start(status: u16, body: impl Into<Bytes>) -> Self {
        let body = body.into();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let queries = Arc::new(Mutex::new(Vec::new()));
        let recorded = queries.clone();
        tokio::spawn(async move {
            loop {
                let (sock, _) = listener.accept().await.unwrap();
                let recorded = recorded.clone();
                let body = body.clone();
                let svc = service_fn(move |req: hyper::Request<_>| {
                    let url = reqwest::Url::parse(&format!("http://x{}", req.uri())).unwrap();
                    recorded
                        .lock()
                        .unwrap()
                        .push(url.query_pairs().into_owned().collect());
                    let body = body.clone();
                    async move {
                        Ok::<_, Infallible>(
                            Response::builder()
                                .status(status)
                                .body(Full::new(body))
                                .unwrap(),
                        )
                    }
                });
                tokio::spawn(http1::Builder::new().serve_connection(TokioIo::new(sock), svc));
            }
        });
        Self { url, queries }
    }

    pub fn queries(&self) -> Vec<HashMap<String, String>> {
        self.queries.lock().unwrap().clone()
    }
}
