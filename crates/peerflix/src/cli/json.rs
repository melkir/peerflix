//! JSON output, for programs that run peerflix to search and stream, such as
//! a player's plugin.

use std::{io::Write, sync::Arc, time::Duration};

use librqbit::Session;
use peerflix_core::{
    http::{Pick, Reply, Server},
    play::Listing,
    providers::{Provider, Query, Torrent},
    search::{self, Category},
    torrent::files::{TorrentFile, pick_file},
    util::human_bytes,
};
use serde::Serialize;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// How long a stream goes without a player connected before peerflix exits.
const IDLE: Duration = Duration::from_secs(30);

/// How long peerflix waits for a program to pick a file to stream, in case
/// the program went away.
const PICK_WAIT: Duration = Duration::from_secs(10 * 60);

/// Everything a search found, printed once all sites have answered.
#[derive(Serialize)]
struct Output {
    /// In the order the sites answered, each in the site's own order.
    results: Vec<Found>,
    /// What to say about the search, as the command line's search does.
    summary: String,
}

#[derive(Serialize)]
struct Found {
    site: &'static str,
    #[serde(flatten)]
    torrent: Torrent,
}

/// Searches providers, category's sites, for query in parallel and writes
/// what they found as a JSON object once they've all answered: results, each
/// a torrent and the site it's from, and a summary of the search, such as the
/// sites that didn't answer.
pub async fn print_results(
    w: &mut impl Write,
    client: &search::Client,
    category: Category,
    providers: &[Arc<dyn Provider>],
    query: &str,
) -> anyhow::Result<()> {
    let mut results = Vec::new();
    let failed = search::search(client, providers, &Query::new(query), |site, items| {
        results.extend(items.into_iter().map(|torrent| Found { site, torrent }));
    })
    .await;
    let summary = search::summary(category, query, results.len(), &failed, providers.len());
    serde_json::to_writer(&mut *w, &Output { results, summary })?;
    writeln!(w)?;
    Ok(())
}

/// Writes the torrent's info hash, its files, leaving out padding, eps, the
/// ids of its episodes in order, and the control URL to start streaming from
/// as a JSON line: info_hash, files, each with its index, path and size,
/// episodes and control.
pub fn print_files(
    w: &mut impl Write,
    info_hash: &str,
    files: &[TorrentFile],
    eps: &[usize],
    control: &str,
) -> anyhow::Result<()> {
    #[derive(Serialize)]
    struct File<'a> {
        index: usize,
        path: &'a str,
        size: String,
    }
    #[derive(Serialize)]
    struct Files<'a> {
        info_hash: &'a str,
        files: Vec<File<'a>>,
        episodes: &'a [usize],
        control: &'a str,
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
            info_hash,
            files,
            episodes: eps,
            control,
        },
    )?;
    // The program reads it as soon as it's written, while peerflix waits.
    writeln!(w)?;
    w.flush()?;
    Ok(())
}

/// Streams listing's torrent for a program: prints its files and the
/// control URL of server, waits for the program to pick one there, from
/// picks, the file at default when it names none, and answers with the
/// stream's JSON once it's served, until no player has been connected for
/// IDLE.
pub async fn stream(
    cancel: &CancellationToken,
    session: &Arc<Session>,
    listing: Listing,
    server: &Server,
    picks: mpsc::Receiver<Pick>,
    default: Option<usize>,
) -> anyhow::Result<()> {
    print_files(
        &mut std::io::stdout().lock(),
        &listing.info_hash(),
        &listing.files,
        &listing.episodes,
        &server.control_url(),
    )?;
    let Some((id, reply)) = wait_for_pick(cancel, picks, &listing.files, default).await else {
        return Ok(());
    };
    let playing = match cancel
        .run_until_cancelled(listing.play(session, server, id))
        .await
    {
        None => return Ok(()),
        Some(Err(e)) => {
            let _ = reply.send(Err(format!("{e:#}")));
            return Err(e);
        }
        Some(Ok(playing)) => playing,
    };
    let _ = reply.send(Ok(serde_json::to_string(&playing.stream)?));
    // The program that picked plays it; when it's done, no one is.
    let connections = server.connections();
    let player = async {
        connections.idle(IDLE).await;
        Ok(())
    };
    playing.watch(cancel, player, |_| {}).await
}

/// Waits for a program to pick a file with a PUT to the control, the file at
/// default or pick_file's choice when it names none, turning down picks of
/// files there aren't. Returns the file picked and where to send the stream's
/// JSON once it's served, or None when cancelled or once PICK_WAIT passes
/// without a pick.
async fn wait_for_pick(
    cancel: &CancellationToken,
    mut picks: mpsc::Receiver<Pick>,
    files: &[TorrentFile],
    default: Option<usize>,
) -> Option<(usize, Reply)> {
    let deadline = tokio::time::sleep(PICK_WAIT);
    tokio::pin!(deadline);
    loop {
        let pick = tokio::select! {
            _ = cancel.cancelled() => return None,
            _ = &mut deadline => {
                eprintln!("No file was picked in {} minutes", PICK_WAIT.as_secs() / 60);
                return None;
            }
            pick = picks.recv() => pick?,
        };
        match pick_file(files, pick.index.or(default)) {
            Ok(id) => return Some((id, pick.reply)),
            Err(e) => {
                let _ = pick.reply.send(Err(format!("{e:#}")));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn waits_for_a_file_that_exists() {
        let files: Vec<_> = ["a.mkv", "b.mkv"]
            .map(|path| TorrentFile {
                path: path.into(),
                len: 1,
                padding: false,
            })
            .into();
        let cancel = CancellationToken::new();
        let (picks, received) = mpsc::channel(2);
        let pick = |index| {
            let (reply, answer) = tokio::sync::oneshot::channel();
            (Pick { index, reply }, answer)
        };
        let (bad, bad_answer) = pick(Some(9));
        let (good, _) = pick(None);
        picks.send(bad).await.unwrap();
        picks.send(good).await.unwrap();
        // A pick of no file in particular streams --index's.
        let (id, _) = wait_for_pick(&cancel, received, &files, Some(1))
            .await
            .unwrap();
        assert_eq!(id, 1);
        assert!(
            bad_answer
                .await
                .unwrap()
                .unwrap_err()
                .contains("out of range")
        );

        let (_picks, received) = mpsc::channel(1);
        cancel.cancel();
        assert!(
            wait_for_pick(&cancel, received, &files, None)
                .await
                .is_none()
        );
    }

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
            summary: "tpb didn't answer.".into(),
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
                "summary": "tpb didn't answer.",
            })
        );
    }

    #[tokio::test]
    async fn prints_one_line() {
        let mut buf = Vec::new();
        let client = search::client().unwrap();
        print_results(&mut buf, &client, Category::Movies, &[], "x")
            .await
            .unwrap();
        assert_eq!(
            String::from_utf8(buf).unwrap(),
            "{\"results\":[],\"summary\":\"\"}\n"
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
        print_files(
            &mut buf,
            "aa",
            &files,
            &[0, 2],
            "http://127.0.0.1:1/peerflix/stream",
        )
        .unwrap();
        let out: serde_json::Value = serde_json::from_slice(&buf).unwrap();
        assert_eq!(
            out,
            serde_json::json!({
                "info_hash": "aa",
                "files": [
                    {"index": 0, "path": "Pioneer.One.S01E01.mkv", "size": "400.0 MiB"},
                    {"index": 2, "path": "Pioneer.One.S01E02.mkv", "size": "400.0 MiB"},
                ],
                "episodes": [0, 2],
                "control": "http://127.0.0.1:1/peerflix/stream",
            })
        );
    }
}
