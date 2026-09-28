use anyhow::{Context, bail};
use serde::Deserialize;

use crate::search::{Torrent, human_bytes, magnet};

/// YTS's API; the yts.* sites point clients here.
pub const YTS_URL: &str = "https://movies-api.accel.li";

#[derive(Deserialize)]
struct Response {
    status: String,
    #[serde(default)]
    status_message: String,
    #[serde(default)]
    data: Data,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Data {
    movies: Vec<Movie>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Movie {
    title_long: String,
    torrents: Vec<Item>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Item {
    hash: String,
    quality: String,
    #[serde(rename = "type")]
    kind: String,
    video_codec: String,
    seeds: u32,
    peers: u32,
    size_bytes: u64,
    date_uploaded: String,
}

/// Searches YTS's movies by title, most seeded first, and returns one result
/// per movie and quality. An empty query returns nothing rather than the
/// whole catalog.
pub async fn search(
    client: &reqwest::Client,
    base: &str,
    query: &str,
) -> anyhow::Result<Vec<Torrent>> {
    if query.trim().is_empty() {
        return Ok(Vec::new());
    }
    let resp = client
        .get(format!("{base}/api/v2/list_movies.json"))
        .query(&[("query_term", query), ("limit", "50"), ("sort_by", "seeds")])
        .send()
        .await
        .context("searching yts")?;
    let status = resp.status();
    if status != reqwest::StatusCode::OK {
        bail!("searching yts: {status}");
    }
    let body: Response = resp.json().await.context("parsing yts results")?;
    if body.status != "ok" {
        bail!("searching yts: {}", body.status_message);
    }
    let mut results = Vec::new();
    for movie in body.data.movies {
        for it in movie.torrents {
            let tags = [&it.quality, &it.kind, &it.video_codec]
                .into_iter()
                .filter(|t| !t.is_empty())
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join(" ");
            let title = format!("{} [{tags}]", movie.title_long);
            results.push(Torrent {
                url: magnet(&it.hash, &title),
                title,
                date: it
                    .date_uploaded
                    .get(..10)
                    .unwrap_or("0001-01-01")
                    .to_owned(),
                size: human_bytes(it.size_bytes),
                info_hash: it.hash.to_ascii_lowercase(),
                seeders: it.seeds,
                leechers: it.peers,
            });
        }
    }
    Ok(results)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::{search::TRACKERS, testutil::FakeServer};

    pub const SAMPLE_JSON: &str = r#"{"status":"ok","status_message":"Query was successful","data":{"movie_count":1,"movies":[
        {"title_long":"Big Buck Bunny (2008)","torrents":[
            {"hash":"0123456789ABCDEF0123456789ABCDEF01234567","quality":"1080p","type":"bluray","video_codec":"x264","seeds":12,"peers":3,"size_bytes":1986422374,"date_uploaded":"2015-11-01 00:14:39"},
            {"hash":"89ABCDEF0123456789ABCDEF0123456789ABCDEF","quality":"2160p","type":"web","seeds":0,"peers":0,"size_bytes":0,"date_uploaded":""}
        ]}]}}"#;

    #[tokio::test]
    async fn search_parses_movies() {
        let srv = FakeServer::start(200, SAMPLE_JSON).await;
        let items = search(&reqwest::Client::new(), &srv.url, "big buck bunny")
            .await
            .unwrap();
        assert_eq!(items.len(), 2);
        let it = &items[0];
        assert_eq!(it.title, "Big Buck Bunny (2008) [1080p bluray x264]");
        assert_eq!((it.seeders, it.leechers), (12, 3));
        assert_eq!(it.size, "1.8 GiB");
        assert_eq!(it.date, "2015-11-01");
        assert_eq!(it.info_hash, "0123456789abcdef0123456789abcdef01234567");
        assert_eq!(items[1].title, "Big Buck Bunny (2008) [2160p web]");
        assert_eq!(items[1].date, "0001-01-01");

        let m = reqwest::Url::parse(&it.url).unwrap();
        assert_eq!(m.scheme(), "magnet");
        let pairs: Vec<(String, String)> = m.query_pairs().into_owned().collect();
        assert_eq!(
            pairs[0],
            (
                "xt".into(),
                "urn:btih:0123456789ABCDEF0123456789ABCDEF01234567".into()
            )
        );
        assert_eq!(pairs[1], ("dn".into(), it.title.clone()));
        let trackers: Vec<_> = pairs[2..].iter().map(|(_, v)| v.as_str()).collect();
        assert_eq!(trackers, TRACKERS);

        let q = srv.queries().remove(0);
        for (k, want) in [("query_term", "big buck bunny"), ("sort_by", "seeds")] {
            assert_eq!(q.get(k).map(String::as_str), Some(want), "query {k}");
        }
    }

    #[tokio::test]
    async fn empty_query_skips_request() {
        let srv = FakeServer::start(200, SAMPLE_JSON).await;
        let items = search(&reqwest::Client::new(), &srv.url, " ")
            .await
            .unwrap();
        assert!(items.is_empty());
        assert!(srv.queries().is_empty());
    }

    #[tokio::test]
    async fn search_errors() {
        let srv = FakeServer::start(200, r#"{"status":"error","status_message":"nope"}"#).await;
        let err = search(&reqwest::Client::new(), &srv.url, "x")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("nope"), "{err}");

        let srv = FakeServer::start(429, "").await;
        let err = search(&reqwest::Client::new(), &srv.url, "x")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("429"), "{err}");
    }
}
