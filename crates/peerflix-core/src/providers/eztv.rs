use anyhow::{Context, anyhow};
use futures_util::future::{BoxFuture, join_all};
use serde::Deserialize;

use crate::{
    providers::{Episode, Provider, Query, Torrent, get_json},
    util::unix_date,
};

pub const EZTV_URL: &str = "https://eztvx.to";
/// EZTV's API only looks shows up by IMDb ID, so titles go through IMDb's
/// search suggestions first.
pub const IMDB_URL: &str = "https://v3.sg.media-imdb.com";

/// The results fetched per request, EZTV's maximum.
const PAGE_SIZE: usize = 100;
/// The most pages fetched when looking for one season.
const MAX_PAGES: usize = 5;

#[derive(Default, Deserialize)]
#[serde(default)]
struct Page {
    torrents_count: usize,
    torrents: Vec<Item>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Item {
    hash: String,
    magnet_url: String,
    title: String,
    season: String,
    episode: String,
    seeds: u32,
    peers: u32,
    size_bytes: String,
    date_released_unix: i64,
}

/// EZTV's TV shows.
pub struct Eztv {
    pub base: String,
    /// IMDb's title suggestions, which map queries to show IDs.
    pub imdb: String,
}

impl Provider for Eztv {
    fn name(&self) -> &'static str {
        "eztv"
    }

    /// Searches EZTV for the query's IMDb ID, or else the show IMDb suggests
    /// for its title, newest first, keeping only the query's season or
    /// episode if it has one.
    fn search<'a>(
        &'a self,
        client: &'a reqwest::Client,
        query: &'a Query,
    ) -> BoxFuture<'a, anyhow::Result<Vec<Torrent>>> {
        Box::pin(async move {
            if query.title.is_empty() {
                return Ok(Vec::new());
            }
            let id = match &query.imdb {
                Some(id) => id.clone(),
                None => match show_id(client, &self.imdb, &query.title).await? {
                    Some(id) => id,
                    None => return Ok(Vec::new()),
                },
            };
            let wanted = query.episode;
            let pages = if wanted.is_some() { MAX_PAGES } else { 1 };
            let items = torrents(client, &self.base, &id, pages).await?;
            Ok(items
                .into_iter()
                .filter(|it| wanted.is_none_or(|w| is_episode(it, w)))
                .map(Torrent::from)
                .collect())
        })
    }
}

/// Returns the IMDb ID, such as tt1234567, of the first TV show IMDb
/// suggests for query, if any.
async fn show_id(
    client: &reqwest::Client,
    base: &str,
    query: &str,
) -> anyhow::Result<Option<String>> {
    #[derive(Deserialize)]
    struct Suggestions {
        #[serde(default)]
        d: Vec<Suggestion>,
    }
    #[derive(Deserialize)]
    struct Suggestion {
        id: String,
        #[serde(default)]
        qid: String,
    }
    let mut url = reqwest::Url::parse(base).context("parsing the IMDb URL")?;
    url.path_segments_mut()
        .map_err(|()| anyhow!("invalid IMDb URL {base:?}"))?
        .pop_if_empty()
        .extend(["suggestion", "x", &format!("{}.json", query.to_lowercase())]);
    let s: Suggestions = get_json(client.get(url), "imdb").await?;
    Ok(s.d
        .into_iter()
        .find(|s| matches!(s.qid.as_str(), "tvSeries" | "tvMiniSeries"))
        .map(|s| s.id))
}

/// Fetches up to max_pages pages of the show's torrents, newest first; pages
/// after the first are fetched concurrently.
async fn torrents(
    client: &reqwest::Client,
    base: &str,
    id: &str,
    max_pages: usize,
) -> anyhow::Result<Vec<Item>> {
    let first = page(client, base, id, 1).await?;
    let pages = first.torrents_count.div_ceil(PAGE_SIZE).min(max_pages);
    let mut items = first.torrents;
    let later = join_all((2..=pages).map(|n| page(client, base, id, n))).await;
    // Later pages are a bonus; keep what arrived.
    for p in later.into_iter().flatten() {
        items.extend(p.torrents);
    }
    Ok(items)
}

async fn page(client: &reqwest::Client, base: &str, id: &str, n: usize) -> anyhow::Result<Page> {
    let req = client.get(format!("{base}/api/get-torrents")).query(&[
        ("imdb_id", id.trim_start_matches("tt")),
        ("limit", &PAGE_SIZE.to_string()),
        ("page", &n.to_string()),
    ]);
    get_json(req, "eztv").await
}

/// Whether it is from wanted's season, or is wanted's episode.
fn is_episode(it: &Item, wanted: Episode) -> bool {
    it.season.parse().is_ok_and(|season| {
        wanted.includes(Episode {
            season,
            episode: it.episode.parse().ok(),
        })
    })
}

impl From<Item> for Torrent {
    fn from(it: Item) -> Self {
        let title = it.title.strip_suffix(" EZTV").unwrap_or(&it.title);
        Torrent {
            title: title.to_owned(),
            url: it.magnet_url,
            date: unix_date(it.date_released_unix),
            size: it.size_bytes.parse().unwrap_or(0),
            seeders: it.seeds,
            leechers: it.peers,
            info_hash: it.hash.to_ascii_lowercase(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::FakeServer;

    const SAMPLE_JSON: &str = r#"{"imdb_id":"1234567","torrents_count":2,"limit":100,"page":1,"torrents":[
        {"hash":"A017AC9BF02DE9E36F1F9177BDB60612186B0B0D","magnet_url":"magnet:?xt=urn:btih:a017ac9bf02de9e36f1f9177bdb60612186b0b0d","title":"Pioneer One S01E10 2160p WEB x265-GRP EZTV","season":"1","episode":"10","seeds":7,"peers":2,"size_bytes":"1726335609","date_released_unix":1790612897},
        {"magnet_url":"magnet:?xt=urn:btih:b017ac9bf02de9e36f1f9177bdb60612186b0b0d","title":"Pioneer One S02E01 720p","season":"2","episode":"1","seeds":0,"peers":0,"size_bytes":"","date_released_unix":0}
    ]}"#;

    #[tokio::test]
    async fn parses_torrents() {
        let srv = FakeServer::start(200, SAMPLE_JSON).await;
        let items = torrents(&reqwest::Client::new(), &srv.url, "tt1234567", 1)
            .await
            .unwrap();
        assert_eq!(items.len(), 2);
        let it = Torrent::from(items.into_iter().next().unwrap());
        assert_eq!(it.title, "Pioneer One S01E10 2160p WEB x265-GRP");
        assert_eq!(
            it.url,
            "magnet:?xt=urn:btih:a017ac9bf02de9e36f1f9177bdb60612186b0b0d"
        );
        assert_eq!((it.seeders, it.leechers), (7, 2));
        assert_eq!(it.size, 1_726_335_609);
        assert_eq!(it.date, "2026-09-28");
        assert_eq!(it.info_hash, "a017ac9bf02de9e36f1f9177bdb60612186b0b0d");

        let q = srv.queries().remove(0);
        for (k, want) in [("imdb_id", "1234567"), ("limit", "100"), ("page", "1")] {
            assert_eq!(q.get(k).map(String::as_str), Some(want), "query {k}");
        }
    }

    #[tokio::test]
    async fn empty_query_skips_request() {
        let srv = FakeServer::start(200, SAMPLE_JSON).await;
        let eztv = Eztv {
            base: srv.url.clone(),
            imdb: srv.url.clone(),
        };
        let items = eztv
            .search(&reqwest::Client::new(), &Query::new(" "))
            .await
            .unwrap();
        assert!(items.is_empty());
        assert!(srv.queries().is_empty());
    }

    #[tokio::test]
    async fn searches_imdb_ids_directly() {
        let srv = FakeServer::start(200, SAMPLE_JSON).await;
        let eztv = Eztv {
            base: srv.url.clone(),
            imdb: srv.url.clone(),
        };
        let items = eztv
            .search(&reqwest::Client::new(), &Query::new("tt1234567 s02"))
            .await
            .unwrap();
        let titles: Vec<_> = items.iter().map(|it| it.title.as_str()).collect();
        assert_eq!(titles, ["Pioneer One S02E01 720p"]);
        // No IMDb lookup, straight to the show's torrents.
        assert_eq!(srv.paths(), ["/api/get-torrents"]);
        assert_eq!(
            srv.queries()[0].get("imdb_id").map(String::as_str),
            Some("1234567")
        );
    }

    #[tokio::test]
    async fn filters_by_episode() {
        let srv = FakeServer::start(200, SAMPLE_JSON).await;
        let client = reqwest::Client::new();
        let items = torrents(&client, &srv.url, "tt1234567", MAX_PAGES)
            .await
            .unwrap();
        let episode = |q| Query::new(q).episode.unwrap();
        let wanted = episode("s02");
        let kept: Vec<_> = items.iter().filter(|it| is_episode(it, wanted)).collect();
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].title, "Pioneer One S02E01 720p");
        let wanted = episode("1x10");
        assert!(is_episode(&items[0], wanted) && !is_episode(&items[1], wanted));
    }

    #[tokio::test]
    async fn finds_show_id() {
        let srv = FakeServer::start(
            200,
            r#"{"d":[{"id":"tt1727587","l":"Sintel","qid":"short"},{"id":"tt7654321","l":"Pioneer One","qid":"tvSeries"}],"q":"x"}"#,
        )
        .await;
        let id = show_id(&reqwest::Client::new(), &srv.url, "Pioneer")
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("tt7654321"));

        let srv = FakeServer::start(200, r#"{"d":[],"q":"x"}"#).await;
        let id = show_id(&reqwest::Client::new(), &srv.url, "nothing")
            .await
            .unwrap();
        assert_eq!(id, None);
    }
}
