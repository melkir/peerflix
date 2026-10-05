//! The torrent sites, and what each implements to be searched: the query it
//! gets, the results it returns, and the Provider trait itself.

pub mod eztv;
pub mod nyaa;
pub mod tpb;
pub mod yts;

use anyhow::{Context, anyhow};
use futures_util::future::BoxFuture;
use serde::{Serialize, Serializer, de::DeserializeOwned};

use crate::util::{human_bytes, parse_digits};

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
    /// The IMDb ID, such as tt1748166, when that's what the title is.
    pub imdb: Option<String>,
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
            imdb: is_imdb_id(title).then(|| title.to_ascii_lowercase()),
            episode,
        }
    }
}

/// Whether s is an IMDb ID: tt and at least 7 digits, in any case.
fn is_imdb_id(s: &str) -> bool {
    s.get(..2).is_some_and(|p| p.eq_ignore_ascii_case("tt"))
        && s.len() >= 9
        && s[2..].bytes().all(|b| b.is_ascii_digit())
}

/// A whole season, or one episode of it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Episode {
    pub season: u32,
    pub episode: Option<u32>,
}

impl Episode {
    /// The first season or episode tag in a release's name, such as the
    /// S01E02 in Show.S01E02.720p, or the Season 2 in Show Season 2 Complete.
    pub fn in_name(name: &str) -> Option<Episode> {
        let words: Vec<_> = name
            .split(|c: char| !c.is_alphanumeric())
            .filter(|w| !w.is_empty())
            .collect();
        words.iter().enumerate().find_map(|(i, w)| {
            if w.eq_ignore_ascii_case("season") {
                let season = words.get(i + 1)?.parse().ok()?;
                return Some(Episode {
                    season,
                    episode: None,
                });
            }
            parse_episode(w)
        })
    }

    /// Whether found is this episode, or in this season if this is a whole
    /// season.
    pub fn includes(self, found: Episode) -> bool {
        self.season == found.season && self.episode.is_none_or(|e| found.episode == Some(e))
    }
}

/// Parses S01E02, S01 or 1x02, in any case.
fn parse_episode(s: &str) -> Option<Episode> {
    let s = s.to_ascii_lowercase();
    let num = parse_digits::<u32>;
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
#[derive(Debug, Default, Serialize)]
pub struct Torrent {
    /// A .torrent URL or magnet link.
    pub url: String,
    pub title: String,
    /// YYYY-MM-DD
    pub date: String,
    /// In bytes, or 0 if the site doesn't say. JSON tells it as human_bytes
    /// does, such as 1.2 GiB.
    #[serde(serialize_with = "human_size")]
    pub size: u64,
    pub seeders: u32,
    pub leechers: u32,
    /// The info hash in lowercase hex, or empty if the site doesn't give
    /// one.
    pub info_hash: String,
}

fn human_size<S: Serializer>(bytes: &u64, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&human_bytes(*bytes))
}

/// Sends req to site and parses its JSON answer, failing unless the site
/// answered 200 OK.
pub async fn get_json<T: DeserializeOwned>(
    req: reqwest::RequestBuilder,
    site: &str,
) -> anyhow::Result<T> {
    get_ok(req, site, |_| None)
        .await?
        .json()
        .await
        .with_context(|| format!("parsing {site} results"))
}

/// Sends req to site and returns its answer, failing unless the site
/// answered 200 OK, with the error explain gives for a status, if any.
pub async fn get_ok(
    req: reqwest::RequestBuilder,
    site: &str,
    explain: impl FnOnce(reqwest::StatusCode) -> Option<anyhow::Error>,
) -> anyhow::Result<reqwest::Response> {
    let resp = req
        .send()
        .await
        .with_context(|| format!("searching {site}"))?;
    let status = resp.status();
    if status != reqwest::StatusCode::OK {
        return Err(explain(status).unwrap_or_else(|| anyhow!("searching {site}: {status}")));
    }
    Ok(resp)
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
            assert_eq!(query.imdb, None, "{q:?}");
        }
    }

    #[test]
    fn spots_imdb_ids() {
        for (q, want) in [
            ("tt1748166", Some("tt1748166")),
            ("TT1748166 s01e02", Some("tt1748166")),
            ("tt12345678", Some("tt12345678")),
            ("tt174816", None),
            ("tt1748166x", None),
            ("pioneer one tt1748166", None),
            ("", None),
        ] {
            assert_eq!(Query::new(q).imdb.as_deref(), want, "{q:?}");
        }
    }

    #[test]
    fn finds_episodes_in_names() {
        let ep = |season, episode| Some(Episode { season, episode });
        for (name, want) in [
            ("Pioneer.One.S01E02.720p.x264", ep(1, Some(2))),
            ("Pioneer One S02 Complete 1080p", ep(2, None)),
            ("Show 3x04 HDTV", ep(3, Some(4))),
            ("Pioneer One Season 1 Complete 720p", ep(1, None)),
            ("Pioneer.One.Season.3.720p.S03E01", ep(3, None)),
            ("The Season Finale", None),
            ("Sintel.2010.1080p", None),
        ] {
            assert_eq!(Episode::in_name(name), want, "{name:?}");
        }
    }

    #[test]
    fn includes_episodes() {
        let ep = |season, episode| Episode { season, episode };
        assert!(ep(1, None).includes(ep(1, Some(3))));
        assert!(ep(1, None).includes(ep(1, None)));
        assert!(ep(1, Some(3)).includes(ep(1, Some(3))));
        assert!(!ep(1, Some(3)).includes(ep(1, Some(4))));
        // A season pack isn't the episode asked for.
        assert!(!ep(1, Some(3)).includes(ep(1, None)));
        assert!(!ep(1, None).includes(ep(2, Some(3))));
    }
}
