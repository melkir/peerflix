//! What every torrent site implements to be searched: the query it gets, the
//! results it returns, and the Provider trait itself.

use anyhow::{Context, bail};
use futures_util::future::BoxFuture;
use serde::de::DeserializeOwned;

/// A site that can be searched for torrents.
pub trait Provider: Send + Sync {
    /// The site's short name, shown next to its results and in errors.
    fn name(&self) -> &'static str;

    /// Searches the site for query. Results come in the site's own order, dead
    /// torrents and all; an error means the site couldn't be searched.
    fn search<'a>(
        &'a self,
        client: &'a reqwest::Client,
        query: &'a Query,
    ) -> BoxFuture<'a, anyhow::Result<Vec<Torrent>>>;
}

/// What to search for.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Query {
    /// The search terms as typed.
    pub text: String,
    /// The terms without a trailing episode, trimmed.
    pub title: String,
    /// The season or episode that trailed the terms, as in S01E03, S01 or
    /// 1x03.
    pub episode: Option<Episode>,
}

impl Query {
    pub fn new(text: &str) -> Self {
        let trimmed = text.trim();
        let (title, last) = trimmed.rsplit_once(' ').unwrap_or(("", trimmed));
        let (title, episode) = match parse_episode(last) {
            Some(e) => (title.trim_end(), Some(e)),
            None => (trimmed, None),
        };
        Query {
            text: text.to_owned(),
            title: title.to_owned(),
            episode,
        }
    }
}

/// A whole season, or one episode of it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Episode {
    pub season: u32,
    pub episode: Option<u32>,
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

/// A search result from any site.
#[derive(Debug, Default)]
pub struct Torrent {
    /// A .torrent URL or magnet link.
    pub url: String,
    pub title: String,
    /// YYYY-MM-DD
    pub date: String,
    pub size: String,
    pub seeders: u32,
    pub leechers: u32,
    /// The info hash in lowercase hex, or empty if the site doesn't give
    /// one.
    pub info_hash: String,
}

/// Sends req to site and parses its JSON answer, failing unless the site
/// answered 200 OK.
pub async fn get_json<T: DeserializeOwned>(
    req: reqwest::RequestBuilder,
    site: &str,
) -> anyhow::Result<T> {
    let resp = req
        .send()
        .await
        .with_context(|| format!("searching {site}"))?;
    let status = resp.status();
    if status != reqwest::StatusCode::OK {
        bail!("searching {site}: {status}");
    }
    resp.json()
        .await
        .with_context(|| format!("parsing {site} results"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_episodes() {
        let ep = |season, episode| Some(Episode { season, episode });
        for (q, title, want) in [
            ("big buck bunny", "big buck bunny", None),
            ("big buck bunny S01E02", "big buck bunny", ep(1, Some(2))),
            ("big buck bunny s3", "big buck bunny", ep(3, None)),
            ("big buck bunny 2x10 ", "big buck bunny", ep(2, Some(10))),
            ("sintel", "sintel", None),
            ("se10", "se10", None),
            ("s01", "", ep(1, None)),
            ("the show s01e", "the show s01e", None),
        ] {
            let query = Query::new(q);
            assert_eq!(query.text, q);
            assert_eq!(
                (query.title.as_str(), query.episode),
                (title, want),
                "{q:?}"
            );
        }
    }
}
