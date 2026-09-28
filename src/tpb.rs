use anyhow::{Context, bail};
use serde::Deserialize;

use crate::search::{Torrent, human_bytes, magnet, unix_date};

/// The Pirate Bay's JSON API.
pub const TPB_URL: &str = "https://apibay.org";

/// The Video category and its subcategories (movies, TV, HD, 4K...).
const VIDEO: &str = "200";

/// apibay sends every field as a string.
#[derive(Default, Deserialize)]
#[serde(default)]
struct Item {
    name: String,
    info_hash: String,
    seeders: String,
    leechers: String,
    size: String,
    added: String,
}

/// Searches The Pirate Bay's video torrents, most seeded first. An empty
/// query returns nothing.
pub async fn search(
    client: &reqwest::Client,
    base: &str,
    query: &str,
) -> anyhow::Result<Vec<Torrent>> {
    if query.trim().is_empty() {
        return Ok(Vec::new());
    }
    let resp = client
        .get(format!("{base}/q.php"))
        .query(&[("q", query), ("cat", VIDEO)])
        .send()
        .await
        .context("searching tpb")?;
    let status = resp.status();
    if status != reqwest::StatusCode::OK {
        bail!("searching tpb: {status}");
    }
    let items: Vec<Item> = resp.json().await.context("parsing tpb results")?;
    Ok(items
        .into_iter()
        // No results come back as one placeholder with an all-zero hash.
        .filter(|it| it.info_hash.bytes().any(|b| b != b'0'))
        .map(|it| Torrent {
            url: magnet(&it.info_hash, &it.name),
            date: unix_date(it.added.parse().unwrap_or(0)),
            size: human_bytes(it.size.parse().unwrap_or(0)),
            seeders: it.seeders.parse().unwrap_or(0),
            leechers: it.leechers.parse().unwrap_or(0),
            title: it.name,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::FakeServer;

    const SAMPLE_JSON: &str = r#"[
        {"id":"7349754","name":"Big Buck Bunny (2008) 1080p BrRip x264","info_hash":"224BF45881252643DFC2E71ABC7B2660A21C68C4","leechers":"94","seeders":"892","size":"1991613584","num_files":"6","username":"someone","added":"1339547627","status":"vip","category":"207","imdb":"tt1254207"},
        {"id":"2","name":"Big Buck Bunny 720p","info_hash":"89ABCDEF0123456789ABCDEF0123456789ABCDEF","leechers":"","seeders":"0","size":"x","added":"0"}
    ]"#;

    const NO_RESULTS: &str = r#"[{"id":"0","name":"No results returned","info_hash":"0000000000000000000000000000000000000000","leechers":"0","seeders":"0","num_files":"0","size":"0","username":"","added":"0","status":"member","category":"0","imdb":"","total_found":"1"}]"#;

    #[tokio::test]
    async fn search_parses_results() {
        let srv = FakeServer::start(200, SAMPLE_JSON).await;
        let items = search(&reqwest::Client::new(), &srv.url, "big buck bunny")
            .await
            .unwrap();
        assert_eq!(items.len(), 2);
        let it = &items[0];
        assert_eq!(it.title, "Big Buck Bunny (2008) 1080p BrRip x264");
        assert!(
            it.url
                .starts_with("magnet:?xt=urn:btih:224BF45881252643DFC2E71ABC7B2660A21C68C4&dn="),
            "{}",
            it.url
        );
        assert_eq!((it.seeders, it.leechers), (892, 94));
        assert_eq!(it.size, "1.9 GiB");
        assert_eq!(it.date, "2012-06-13");
        let bad = &items[1];
        assert_eq!(
            (bad.seeders, bad.leechers, bad.size.as_str()),
            (0, 0, "0 B")
        );

        let q = srv.queries().remove(0);
        for (k, want) in [("q", "big buck bunny"), ("cat", "200")] {
            assert_eq!(q.get(k).map(String::as_str), Some(want), "query {k}");
        }
    }

    #[tokio::test]
    async fn no_results() {
        let srv = FakeServer::start(200, NO_RESULTS).await;
        let items = search(&reqwest::Client::new(), &srv.url, "nothing")
            .await
            .unwrap();
        assert!(items.is_empty());

        let items = search(&reqwest::Client::new(), &srv.url, "").await.unwrap();
        assert!(items.is_empty());
        assert_eq!(srv.queries().len(), 1, "empty query was sent");
    }

    #[tokio::test]
    async fn search_errors() {
        let srv = FakeServer::start(502, "").await;
        let err = search(&reqwest::Client::new(), &srv.url, "x")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("502"), "{err}");
    }
}
