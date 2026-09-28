use anyhow::{Context, bail};
use serde::{Deserialize, Deserializer};

use crate::search::{Torrent, human_bytes, magnet, unix_date};

/// The Pirate Bay's JSON API.
pub const TPB_URL: &str = "https://apibay.org";

/// Which of The Pirate Bay's video categories to search.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Movies,
    Tv,
}

impl Kind {
    /// The category whose top 100 lists what's popular: HD movies or HD TV
    /// shows.
    fn top(self) -> &'static str {
        match self {
            Kind::Movies => "207",
            Kind::Tv => "208",
        }
    }

    /// The category IDs: SD, HD and 4K movies or TV shows, leaving out
    /// DVD images and 3D.
    fn categories(self) -> &'static str {
        match self {
            Kind::Movies => "201,207,211",
            Kind::Tv => "205,208,212",
        }
    }
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Item {
    name: String,
    info_hash: String,
    #[serde(deserialize_with = "number")]
    seeders: u64,
    #[serde(deserialize_with = "number")]
    leechers: u64,
    #[serde(deserialize_with = "number")]
    size: u64,
    #[serde(deserialize_with = "number")]
    added: i64,
}

/// Searches The Pirate Bay's movies or TV shows, most seeded first. An empty
/// query lists the top 100 in HD, what's popular right now.
pub async fn search(
    client: &reqwest::Client,
    base: &str,
    query: &str,
    kind: Kind,
) -> anyhow::Result<Vec<Torrent>> {
    let req = if query.trim().is_empty() {
        client.get(format!(
            "{base}/precompiled/data_top100_{}.json",
            kind.top()
        ))
    } else {
        client
            .get(format!("{base}/q.php"))
            .query(&[("q", query), ("cat", kind.categories())])
    };
    let resp = req.send().await.context("searching tpb")?;
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
            date: unix_date(it.added),
            size: human_bytes(it.size),
            seeders: it.seeders.try_into().unwrap_or(u32::MAX),
            leechers: it.leechers.try_into().unwrap_or(u32::MAX),
            info_hash: it.info_hash.to_ascii_lowercase(),
            title: it.name,
        })
        .collect())
}

/// Reads a number, which apibay sends as a string in search results and as
/// a number in its top 100 lists. Anything else is 0.
fn number<'de, D, N>(d: D) -> Result<N, D::Error>
where
    D: Deserializer<'de>,
    N: Deserialize<'de> + std::str::FromStr + Default,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Value<N> {
        Number(N),
        Text(String),
        Other(serde::de::IgnoredAny),
    }
    Ok(match Value::<N>::deserialize(d)? {
        Value::Number(n) => n,
        Value::Text(s) => s.parse().unwrap_or_default(),
        Value::Other(_) => N::default(),
    })
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
        let items = search(
            &reqwest::Client::new(),
            &srv.url,
            "big buck bunny",
            Kind::Movies,
        )
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
        assert_eq!(it.info_hash, "224bf45881252643dfc2e71abc7b2660a21c68c4");
        let bad = &items[1];
        assert_eq!(
            (bad.seeders, bad.leechers, bad.size.as_str()),
            (0, 0, "0 B")
        );

        let q = srv.queries().remove(0);
        for (k, want) in [("q", "big buck bunny"), ("cat", "201,207,211")] {
            assert_eq!(q.get(k).map(String::as_str), Some(want), "query {k}");
        }
    }

    #[tokio::test]
    async fn no_results() {
        let srv = FakeServer::start(200, NO_RESULTS).await;
        let items = search(&reqwest::Client::new(), &srv.url, "nothing", Kind::Tv)
            .await
            .unwrap();
        assert!(items.is_empty());
        assert_eq!(
            srv.queries()[0].get("cat").map(String::as_str),
            Some("205,208,212")
        );
    }

    /// The top 100 lists send numbers as numbers.
    const TOP_JSON: &str = r#"[
        {"id":83970962,"info_hash":"9D86667F49F42712909C2888D346B37A17C44191","category":207,"name":"Big Buck Bunny 2008 1080p","status":"vip","num_files":1,"size":3808117223,"seeders":6824,"leechers":7648,"username":"Anonymous","added":1785517204,"imdb":null}
    ]"#;

    #[tokio::test]
    async fn empty_query_lists_top_100() {
        let srv = FakeServer::start(200, TOP_JSON).await;
        let client = reqwest::Client::new();
        let items = search(&client, &srv.url, " ", Kind::Movies).await.unwrap();
        assert_eq!(items.len(), 1);
        let it = &items[0];
        assert_eq!((it.seeders, it.leechers), (6824, 7648));
        assert_eq!(it.size, "3.5 GiB");
        assert_eq!(it.date, "2026-07-31");
        search(&client, &srv.url, "", Kind::Tv).await.unwrap();
        assert_eq!(
            srv.paths(),
            [
                "/precompiled/data_top100_207.json",
                "/precompiled/data_top100_208.json"
            ]
        );
    }

    #[tokio::test]
    async fn search_errors() {
        let srv = FakeServer::start(502, "").await;
        let err = search(&reqwest::Client::new(), &srv.url, "x", Kind::Tv)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("502"), "{err}");
    }
}
