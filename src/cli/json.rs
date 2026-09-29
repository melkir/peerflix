//! Search results as JSON, for programs that run peerflix to search, such as
//! a player's plugin.

use std::{io::Write, sync::Arc};

use peerflix::{
    providers::{Provider, Query, Torrent},
    search::{self, Failed},
};
use serde::Serialize;

/// Everything a search found, printed once all sites have answered.
#[derive(Serialize)]
struct Output {
    /// In the order the sites answered, each in the site's own order.
    results: Vec<Found>,
    #[serde(flatten)]
    failed: Failed,
}

#[derive(Serialize)]
struct Found {
    site: &'static str,
    #[serde(flatten)]
    torrent: Torrent,
}

/// Searches providers for query in parallel and writes what they found as a
/// JSON object once they've all answered: results, each a torrent and the
/// site it's from, and the sites that failed, split into unanswered and
/// errors.
pub async fn print_results(
    w: &mut impl Write,
    providers: &[Arc<dyn Provider>],
    query: &str,
) -> anyhow::Result<()> {
    let mut results = Vec::new();
    let failed = search::search(providers, &Query::new(query), |site, items| {
        results.extend(items.into_iter().map(|torrent| Found { site, torrent }));
    })
    .await;
    serde_json::to_writer(&mut *w, &Output { results, failed })?;
    writeln!(w)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_shape() {
        let out = Output {
            results: vec![Found {
                site: "yts",
                torrent: Torrent {
                    url: "magnet:?xt=urn:btih:aa".into(),
                    title: "Sintel (2010) [1080p]".into(),
                    date: "2015-11-01".into(),
                    size: "1.8 GiB".into(),
                    seeders: 12,
                    leechers: 3,
                    info_hash: "aa".into(),
                },
            }],
            failed: Failed {
                unanswered: vec!["tpb"],
                errors: vec![],
            },
        };
        assert_eq!(
            serde_json::to_value(&out).unwrap(),
            serde_json::json!({
                "results": [{
                    "site": "yts",
                    "url": "magnet:?xt=urn:btih:aa",
                    "title": "Sintel (2010) [1080p]",
                    "date": "2015-11-01",
                    "size": "1.8 GiB",
                    "seeders": 12,
                    "leechers": 3,
                    "info_hash": "aa",
                }],
                "unanswered": ["tpb"],
                "errors": [],
            })
        );
    }

    #[tokio::test]
    async fn prints_one_line() {
        let mut buf = Vec::new();
        print_results(&mut buf, &[], "x").await.unwrap();
        assert_eq!(
            String::from_utf8(buf).unwrap(),
            "{\"results\":[],\"unanswered\":[],\"errors\":[]}\n"
        );
    }
}
