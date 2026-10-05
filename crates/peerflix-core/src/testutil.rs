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
/// and records each request's URL.
pub struct FakeServer {
    pub url: String,
    requests: Arc<Mutex<Vec<reqwest::Url>>>,
}

impl FakeServer {
    pub async fn start(status: u16, body: impl Into<Bytes>) -> Self {
        let body = body.into();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = requests.clone();
        tokio::spawn(async move {
            loop {
                let (sock, _) = listener.accept().await.unwrap();
                let recorded = recorded.clone();
                let body = body.clone();
                let svc = service_fn(move |req: hyper::Request<_>| {
                    let url = reqwest::Url::parse(&format!("http://x{}", req.uri())).unwrap();
                    recorded.lock().unwrap().push(url);
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
        Self { url, requests }
    }

    pub fn paths(&self) -> Vec<String> {
        let requests = self.requests.lock().unwrap();
        requests.iter().map(|u| u.path().to_owned()).collect()
    }

    pub fn queries(&self) -> Vec<HashMap<String, String>> {
        let requests = self.requests.lock().unwrap();
        requests
            .iter()
            .map(|u| u.query_pairs().into_owned().collect())
            .collect()
    }
}
