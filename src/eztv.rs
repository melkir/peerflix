use anyhow::{Context, anyhow};
use serde::Deserialize;
use tokio::task::JoinSet;

use crate::search::{Torrent, get_json, human_bytes, unix_date};

pub const EZTV_URL: &str = "https://eztvx.to";
/// EZTV's API only looks shows up by IMDb ID, so titles go through IMDb's
/// search suggestions first.
pub const IMDB_URL: &str = "https://v3.sg.media-imdb.com";

/// The results fetched per request, EZTV's maximum.
const PAGE_SIZE: usize = 100;
/// The most pages fetched when looking for one season.
const MAX_PAGES: usize = 5;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Episode {
    season: u32,
    episode: Option<u32>,
}

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

/// Searches EZTV for the show IMDb suggests for query, newest first. A
/// trailing S01E02, S01 or 1x02 keeps only that season or episode.
pub async fn search(
    client: &reqwest::Client,
    imdb: &str,
    base: &str,
    query: &str,
) -> anyhow::Result<Vec<Torrent>> {
    let (show, wanted) = split_episode(query);
    if show.is_empty() {
        return Ok(Vec::new());
    }
    let Some(id) = show_id(client, imdb, show).await? else {
        return Ok(Vec::new());
    };
    let pages = if wanted.is_some() { MAX_PAGES } else { 1 };
    let items = torrents(client, base, &id, pages).await?;
    Ok(items
        .into_iter()
        .filter(|it| wanted.is_none_or(|w| w.matches(it)))
        .map(Torrent::from)
        .collect())
}

/// Returns the IMDb ID, such as tt0944947, of the first TV show IMDb
/// suggests for query, if any.
async fn show_id(
    client: &reqwest::Client,
    base: &str,
    query: &str,
) -> anyhow::Result<Option<String>> {
    let mut url = reqwest::Url::parse(base).context("parsing the IMDb URL")?;
    url.path_segments_mut()
        .map_err(|()| anyhow!("invalid IMDb URL {base:?}"))?
        .pop_if_empty()
        .extend(["suggestion", "x", &format!("{}.json", query.to_lowercase())]);
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
    let s: Suggestions = get_json(client.get(url), "imdb").await?;
    Ok(s.d
        .into_iter()
        .find(|s| matches!(s.qid.as_str(), "tvSeries" | "tvMiniSeries"))
        .map(|s| s.id))
}

/// Fetches up to max_pages pages of the show's torrents, newest first; pages
/// after the first are fetched in parallel.
async fn torrents(
    client: &reqwest::Client,
    base: &str,
    id: &str,
    max_pages: usize,
) -> anyhow::Result<Vec<Item>> {
    let first = page(client, base, id, 1).await?;
    let pages = first.torrents_count.div_ceil(PAGE_SIZE).min(max_pages);
    let mut items = first.torrents;
    let mut rest = JoinSet::new();
    for n in 2..=pages {
        let (client, base, id) = (client.clone(), base.to_owned(), id.to_owned());
        rest.spawn(async move { (n, page(&client, &base, &id, n).await) });
    }
    let mut later: Vec<_> = rest.join_all().await;
    later.sort_by_key(|(n, _)| *n);
    for (_, p) in later {
        // Later pages are a bonus; keep what arrived.
        if let Ok(p) = p {
            items.extend(p.torrents);
        }
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

impl Episode {
    fn matches(self, it: &Item) -> bool {
        it.season.parse() == Ok(self.season)
            && self.episode.is_none_or(|e| it.episode.parse() == Ok(e))
    }
}

impl From<Item> for Torrent {
    fn from(it: Item) -> Self {
        let title = it.title.strip_suffix(" EZTV").unwrap_or(&it.title);
        Torrent {
            title: title.to_owned(),
            url: it.magnet_url,
            date: unix_date(it.date_released_unix),
            size: human_bytes(it.size_bytes.parse().unwrap_or(0)),
            seeders: it.seeds,
            leechers: it.peers,
            info_hash: it.hash.to_ascii_lowercase(),
        }
    }
}

/// Splits a trailing episode token off query, returning the show's title and
/// the episode.
fn split_episode(query: &str) -> (&str, Option<Episode>) {
    let query = query.trim();
    let (show, last) = query.rsplit_once(' ').unwrap_or(("", query));
    match parse_episode(last) {
        Some(e) => (show.trim_end(), Some(e)),
        None => (query, None),
    }
}

/// Parses S01E02, S01 or 1x02, in any case.
fn parse_episode(s: &str) -> Option<Episode> {
    let s = s.to_ascii_lowercase();
    let num = |t: &str| {
        if t.is_empty() || !t.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        t.parse().ok()
    };
    if let Some(rest) = s.strip_prefix('s') {
        let (season, episode) = match rest.split_once('e') {
            Some((s, e)) => (s, Some(num(e)?)),
            None => (rest, None),
        };
        return Some(Episode {
            season: num(season)?,
            episode,
        });
    }
    let (season, episode) = s.split_once('x')?;
    Some(Episode {
        season: num(season)?,
        episode: Some(num(episode)?),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::FakeServer;

    const SAMPLE_JSON: &str = r#"{"imdb_id":"0944947","torrents_count":2,"limit":100,"page":1,"torrents":[
        {"hash":"A017AC9BF02DE9E36F1F9177BDB60612186B0B0D","magnet_url":"magnet:?xt=urn:btih:a017ac9bf02de9e36f1f9177bdb60612186b0b0d","title":"Game of Thrones S01E10 2160p UHD BluRay x265-SCOTLUHD EZTV","season":"1","episode":"10","seeds":7,"peers":2,"size_bytes":"1726335609","date_released_unix":1790612897},
        {"magnet_url":"magnet:?xt=urn:btih:b017ac9bf02de9e36f1f9177bdb60612186b0b0d","title":"Game of Thrones S02E01 720p","season":"2","episode":"1","seeds":0,"peers":0,"size_bytes":"","date_released_unix":0}
    ]}"#;

    #[tokio::test]
    async fn parses_torrents() {
        let srv = FakeServer::start(200, SAMPLE_JSON).await;
        let items = torrents(&reqwest::Client::new(), &srv.url, "tt0944947", 1)
            .await
            .unwrap();
        assert_eq!(items.len(), 2);
        let it = Torrent::from(items.into_iter().next().unwrap());
        assert_eq!(
            it.title,
            "Game of Thrones S01E10 2160p UHD BluRay x265-SCOTLUHD"
        );
        assert_eq!(
            it.url,
            "magnet:?xt=urn:btih:a017ac9bf02de9e36f1f9177bdb60612186b0b0d"
        );
        assert_eq!((it.seeders, it.leechers), (7, 2));
        assert_eq!(it.size, "1.6 GiB");
        assert_eq!(it.date, "2026-09-28");
        assert_eq!(it.info_hash, "a017ac9bf02de9e36f1f9177bdb60612186b0b0d");

        let q = srv.queries().remove(0);
        for (k, want) in [("imdb_id", "0944947"), ("limit", "100"), ("page", "1")] {
            assert_eq!(q.get(k).map(String::as_str), Some(want), "query {k}");
        }
    }

    #[tokio::test]
    async fn empty_query_skips_request() {
        let srv = FakeServer::start(200, SAMPLE_JSON).await;
        let items = search(&reqwest::Client::new(), &srv.url, &srv.url, " ")
            .await
            .unwrap();
        assert!(items.is_empty());
        assert!(srv.queries().is_empty());
    }

    #[tokio::test]
    async fn filters_by_episode() {
        let srv = FakeServer::start(200, SAMPLE_JSON).await;
        let client = reqwest::Client::new();
        let items = torrents(&client, &srv.url, "tt0944947", MAX_PAGES)
            .await
            .unwrap();
        let wanted = parse_episode("s02").unwrap();
        let kept: Vec<_> = items.iter().filter(|it| wanted.matches(it)).collect();
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].title, "Game of Thrones S02E01 720p");
        let wanted = parse_episode("1x10").unwrap();
        assert!(wanted.matches(&items[0]) && !wanted.matches(&items[1]));
    }

    #[tokio::test]
    async fn finds_show_id() {
        let srv = FakeServer::start(
            200,
            r#"{"d":[{"id":"tt1375666","l":"Inception","qid":"movie"},{"id":"tt11198330","l":"House of the Dragon","qid":"tvSeries"}],"q":"x"}"#,
        )
        .await;
        let id = show_id(&reqwest::Client::new(), &srv.url, "House of the")
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("tt11198330"));

        let srv = FakeServer::start(200, r#"{"d":[],"q":"x"}"#).await;
        let id = show_id(&reqwest::Client::new(), &srv.url, "nothing")
            .await
            .unwrap();
        assert_eq!(id, None);
    }

    #[test]
    fn splits_episodes() {
        let ep = |season, episode| Some(Episode { season, episode });
        for (q, show, want) in [
            ("breaking bad", "breaking bad", None),
            ("breaking bad S01E02", "breaking bad", ep(1, Some(2))),
            ("breaking bad s3", "breaking bad", ep(3, None)),
            ("breaking bad 2x10 ", "breaking bad", ep(2, Some(10))),
            ("suits", "suits", None),
            ("se7en", "se7en", None),
            ("s01", "", ep(1, None)),
            ("the sopranos s01e", "the sopranos s01e", None),
        ] {
            assert_eq!(split_episode(q), (show, want), "{q:?}");
        }
    }
}
