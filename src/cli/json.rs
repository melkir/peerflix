//! JSON output, for programs that run peerflix to search and stream, such as
//! a player's plugin.

use std::{io::Write, sync::Arc};

use peerflix::{
    providers::{Provider, Query, Torrent},
    search::{self, Failed},
    torrent::{Stream, files::TorrentFile},
    util::human_bytes,
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

/// Writes the torrent's files, leaving out padding, and eps, the ids of its
/// episodes in order, as a JSON object: files, each with its index, path and
/// size, and episodes.
pub fn print_files(w: &mut impl Write, files: &[TorrentFile], eps: &[usize]) -> anyhow::Result<()> {
    #[derive(Serialize)]
    struct File<'a> {
        index: usize,
        path: &'a str,
        size: String,
    }
    #[derive(Serialize)]
    struct Files<'a> {
        files: Vec<File<'a>>,
        episodes: &'a [usize],
    }
    let files = files
        .iter()
        .enumerate()
        .filter(|(_, f)| !f.padding)
        .map(|(index, f)| File {
            index,
            path: &f.path,
            size: human_bytes(f.len),
        })
        .collect();
    serde_json::to_writer(
        &mut *w,
        &Files {
            files,
            episodes: eps,
        },
    )?;
    writeln!(w)?;
    Ok(())
}

/// Writes the stream being served as a JSON line: its name and url, its
/// subtitles, each with a name and url, its control URL, to GET its status
/// from or DELETE to stop it, and peerflix's pid.
pub fn print_stream(w: &mut impl Write, stream: &Stream) -> anyhow::Result<()> {
    #[derive(Serialize)]
    struct Subtitle<'a> {
        name: &'a str,
        url: &'a str,
    }
    #[derive(Serialize)]
    struct Served<'a> {
        name: &'a str,
        url: &'a str,
        subtitles: Vec<Subtitle<'a>>,
        control: &'a str,
        pid: u32,
    }
    let subtitles = stream
        .sub_names
        .iter()
        .zip(&stream.sub_urls)
        .map(|(name, url)| Subtitle { name, url })
        .collect();
    serde_json::to_writer(
        &mut *w,
        &Served {
            name: &stream.name,
            url: &stream.url,
            subtitles,
            control: &stream.control_url,
            pid: std::process::id(),
        },
    )?;
    // The program reads it as soon as it's written, while peerflix serves.
    writeln!(w)?;
    w.flush()?;
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

    #[test]
    fn prints_files() {
        let file = |path: &str, len, padding| TorrentFile {
            path: path.into(),
            len,
            padding,
        };
        let files = [
            file("Pioneer.One.S01E01.mkv", 400 << 20, false),
            file(".pad/1", 100, true),
            file("Pioneer.One.S01E02.mkv", 400 << 20, false),
        ];
        let mut buf = Vec::new();
        print_files(&mut buf, &files, &[0, 2]).unwrap();
        let out: serde_json::Value = serde_json::from_slice(&buf).unwrap();
        assert_eq!(
            out,
            serde_json::json!({
                "files": [
                    {"index": 0, "path": "Pioneer.One.S01E01.mkv", "size": "400.0 MiB"},
                    {"index": 2, "path": "Pioneer.One.S01E02.mkv", "size": "400.0 MiB"},
                ],
                "episodes": [0, 2],
            })
        );
    }
}
