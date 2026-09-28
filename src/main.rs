//! peerflix streams a torrent (magnet link, .torrent file or URL) to IINA.
//!
//! The selected file is served over a local HTTP server with range support;
//! reads prioritize the pieces around the player's read position, so playback
//! starts as soon as the first pieces arrive and seeking works.

mod eztv;
mod nyaa;
mod search;
mod stream;
#[cfg(test)]
mod testutil;
mod yts;

use std::{
    path::{Path, PathBuf},
    process::ExitCode,
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, bail};
use clap::Parser;
use librqbit::{
    AddTorrent, AddTorrentOptions, AddTorrentResponse, DhtSessionConfig, ListOnlyResponse,
    ListenerOptions, ManagedTorrent, PeerConnectionOptions, Session, SessionOptions, TorrentStats,
};
use tokio::{net::TcpListener, signal::unix::SignalKind};
use tokio_util::{sync::CancellationToken, task::AbortOnDropHandle};

use crate::{
    search::{Endpoints, NoSelection, Source, human_bytes},
    stream::{Reader, content_type, path_escape},
};

/// The release version, or the crate version for `cargo install` builds.
const VERSION: &str = match option_env!("PEERFLIX_VERSION") {
    Some(v) => v,
    None => env!("CARGO_PKG_VERSION"),
};

/// Stream a torrent straight into IINA.
///
/// With a magnet link, .torrent file or http(s) URL, streams it. Anything else
/// searches nyaa.si, YTS and EZTV interactively in fzf.
#[derive(Parser, Debug)]
#[command(version = VERSION)]
struct Cli {
    /// Magnet link, .torrent file or URL, or search terms
    #[arg(value_name = "SOURCE | QUERY")]
    source: Vec<String>,

    /// HTTP port to serve the stream on (0 = random)
    #[arg(short, long, default_value_t = 8888)]
    port: u16,

    /// Download directory, reused across runs [default: $TMPDIR/peerflix]
    #[arg(short, long)]
    dir: Option<PathBuf>,

    /// File index to stream [default: largest video file]
    #[arg(short, long)]
    index: Option<usize>,

    /// List files in the torrent and exit
    #[arg(short, long)]
    list: bool,

    /// Don't launch IINA, just serve the stream
    #[arg(short, long)]
    no_play: bool,

    /// Sites to search, comma separated [default: all, or nyaa with --user or
    /// --trusted]
    #[arg(
        short = 's',
        long = "source",
        value_name = "SOURCE",
        value_enum,
        value_delimiter = ','
    )]
    sources: Vec<Source>,

    /// Only search torrents from this nyaa uploader (name or profile URL)
    #[arg(short, long)]
    user: Option<String>,

    /// Only search torrents from trusted nyaa uploaders
    #[arg(short, long)]
    trusted: bool,

    /// Print results for the search terms as fzf input and exit
    #[arg(long)]
    print: bool,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("starting the tokio runtime");
    let result = rt.block_on(async_main(cli));
    // Don't wait on librqbit's blocking disk tasks; the next run's data check
    // catches any piece they didn't finish writing.
    rt.shutdown_timeout(Duration::from_secs(1));
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) if e.is::<NoSelection>() => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

async fn async_main(cli: Cli) -> anyhow::Result<()> {
    let user = nyaa_user(cli.user.as_deref().unwrap_or(""));
    let sources = search_sources(&cli.sources, !user.is_empty() || cli.trusted);
    let mut source = cli.source.join(" ");
    if cli.print {
        search::print_results(
            &mut std::io::stdout().lock(),
            &Endpoints::from_env(),
            &sources,
            &source,
            user,
            cli.trusted,
        )
        .await;
        return Ok(());
    }
    if !is_torrent_source(&source) {
        source = search::search_interactive(&source, &sources, user, cli.trusted)?;
    }

    // Installed after fzf, which handles Ctrl-C itself.
    let cancel = CancellationToken::new();
    tokio::spawn(cancel_on_signal(cancel.clone()));
    run(&cancel, &source, &cli).await
}

async fn cancel_on_signal(cancel: CancellationToken) {
    let mut term =
        tokio::signal::unix::signal(SignalKind::terminate()).expect("installing SIGTERM handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
    cancel.cancel();
}

/// Returns the sources to search, without repeats: those given, or else all
/// of them, or only nyaa when nyaa's uploader filters are on.
fn search_sources(given: &[Source], nyaa_filters: bool) -> Vec<Source> {
    if given.is_empty() {
        return if nyaa_filters {
            vec![Source::Nyaa]
        } else {
            Source::ALL.to_vec()
        };
    }
    let mut sources = Vec::new();
    for &s in given {
        if !sources.contains(&s) {
            sources.push(s);
        }
    }
    sources
}

/// Accepts an uploader name or a profile URL such as
/// https://nyaa.si/user/NAME and returns the name.
fn nyaa_user(s: &str) -> &str {
    let s = s.trim_end_matches('/');
    match s.rfind("/user/") {
        Some(i) => &s[i + "/user/".len()..],
        None => s,
    }
}

/// Reports whether s is something librqbit can load rather than search terms.
fn is_torrent_source(s: &str) -> bool {
    ["magnet:", "http://", "https://"]
        .iter()
        .any(|p| s.starts_with(p))
        || (s.ends_with(".torrent") && Path::new(s).is_file())
}

async fn run(cancel: &CancellationToken, source: &str, cli: &Cli) -> anyhow::Result<()> {
    // Kept after exit so a later run resumes from it. macOS clears $TMPDIR
    // of files that go unused for a few days.
    let data_dir = match &cli.dir {
        Some(dir) => dir.clone(),
        None => std::env::temp_dir().join("peerflix"),
    };

    let session = Session::new_with_opts(
        data_dir,
        SessionOptions {
            cancellation_token: Some(cancel.child_token()),
            // Don't leave DHT state behind in the user's cache dir.
            dht: Some(DhtSessionConfig {
                persistence: None,
                ..Default::default()
            }),
            listen: Some(ListenerOptions {
                enable_upnp_port_forwarding: true,
                ..Default::default()
            }),
            ..Default::default()
        },
    )
    .await
    .context("creating torrent session")?;
    let result = stream_torrent(cancel, &session, source, cli).await;
    session.stop().await;
    result
}

struct TorrentFile {
    path: String,
    len: u64,
    /// BEP 47 padding files only exist to align the next file to a piece.
    padding: bool,
}

async fn stream_torrent(
    cancel: &CancellationToken,
    session: &Arc<Session>,
    source: &str,
    cli: &Cli,
) -> anyhow::Result<()> {
    eprintln!("Fetching torrent metadata...");
    let add = AddTorrent::from_cli_argument(source)?;
    let Some(meta) = cancel
        .run_until_cancelled(fetch_metadata(session, add))
        .await
    else {
        return Ok(());
    };
    let meta = meta?;
    let files = torrent_files(&meta);

    if cli.list {
        for (i, f) in files.iter().enumerate().filter(|(_, f)| !f.padding) {
            println!("{i:3}  {:>9}  {}", human_bytes(f.len), f.path);
        }
        return Ok(());
    }

    let id = pick_file(&files, cli.index)?;
    let file = &files[id];
    let name = Path::new(&file.path)
        .file_name()
        .map_or_else(|| file.path.clone(), |n| n.to_string_lossy().into_owned());

    // Download just that file; streams still take priority. With --dir,
    // existing data is checked and reused.
    let opts = AddTorrentOptions {
        only_files: Some(vec![id]),
        overwrite: true,
        initial_peers: Some(meta.seen_peers),
        peer_opts: Some(PeerConnectionOptions {
            // The piece at the player's position after a seek is requested
            // behind everything already queued to a peer. librqbit queues 128
            // chunks (2 MiB); 32 cut long jumps in IINA from 2.6-8 s to
            // 0.5-2.4 s without slowing the download.
            max_request_window: Some(32),
            ..Default::default()
        }),
        ..Default::default()
    };
    let Some(resp) = cancel
        .run_until_cancelled(
            session.add_torrent(AddTorrent::from_bytes(meta.torrent_bytes), Some(opts)),
        )
        .await
    else {
        return Ok(());
    };
    let torrent = resp?.into_handle().context("torrent was not added")?;

    let listener = TcpListener::bind(("127.0.0.1", cli.port))
        .await
        .with_context(|| format!("listening on port {}", cli.port))?;
    let url = format!("http://{}/{}", listener.local_addr()?, path_escape(&name));
    let stream_file = torrent_file(torrent.clone(), id, name.clone(), file.len);
    let _server =
        AbortOnDropHandle::new(tokio::spawn(stream::serve(listener, Arc::new(stream_file))));

    eprintln!("Streaming {name} ({})\n{url}", human_bytes(file.len));

    let player = async {
        if cli.no_play {
            std::future::pending().await
        } else {
            launch_iina(url).await
        }
    };
    tokio::pin!(player);
    let mut ticker = tokio::time::interval_at(
        tokio::time::Instant::now() + Duration::from_secs(1),
        Duration::from_secs(1),
    );
    let mut last_fetched = 0;
    let mut init = torrent.wait_until_initialized();
    let mut initialized = false;
    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                eprintln!();
                return Ok(());
            }
            result = &mut player => {
                eprintln!();
                return result;
            }
            result = &mut init, if !initialized => {
                result.context("checking existing data")?;
                initialized = true;
            }
            _ = ticker.tick() => {
                if !initialized {
                    eprint!("\r\x1b[KChecking existing data...");
                    continue;
                }
                let (line, fetched) = progress_line(&torrent.stats(), id, file.len, last_fetched);
                eprint!("\r\x1b[K{line}");
                last_fetched = fetched;
            }
        }
    }
}

/// Resolves the torrent's metadata without adding it, so nothing is
/// downloaded until a file is picked.
async fn fetch_metadata(
    session: &Arc<Session>,
    add: AddTorrent<'_>,
) -> anyhow::Result<ListOnlyResponse> {
    let opts = AddTorrentOptions {
        list_only: true,
        ..Default::default()
    };
    match session.add_torrent(add, Some(opts)).await? {
        AddTorrentResponse::ListOnly(meta) => Ok(meta),
        _ => bail!("torrent was added instead of listed"),
    }
}

fn torrent_files(meta: &ListOnlyResponse) -> Vec<TorrentFile> {
    meta.info
        .iter_file_details()
        .map(|d| TorrentFile {
            path: d.filename.to_pathbuf().to_string_lossy().into_owned(),
            len: d.len,
            padding: d.attrs().padding,
        })
        .collect()
}

/// Serves file id of torrent. With --dir, existing data is checked first; IINA
/// starts meanwhile and librqbit holds its first request until the check ends.
fn torrent_file(torrent: Arc<ManagedTorrent>, id: usize, name: String, len: u64) -> stream::File {
    stream::File {
        name,
        len,
        open: Box::new(move || {
            let t = torrent.clone();
            Box::pin(async move { Ok(Box::pin(t.stream(id).await?) as Reader) })
        }),
    }
}

/// Returns the status line for file id of len bytes, and the torrent's
/// fetched byte count to pass back as last_fetched on the next tick.
fn progress_line(stats: &TorrentStats, id: usize, len: u64, last_fetched: u64) -> (String, u64) {
    let done = stats.file_progress.get(id).copied().unwrap_or(0);
    let (fetched, live, seen) = stats.live.as_ref().map_or((last_fetched, 0, 0), |l| {
        let s = &l.snapshot;
        (s.fetched_bytes, s.peer_stats.live, s.peer_stats.seen)
    });
    let line = format!(
        "{:5.1}%  {}/s  peers {live}/{seen}",
        100.0 * done as f64 / len.max(1) as f64,
        human_bytes(fetched.saturating_sub(last_fetched)),
    );
    (line, fetched)
}

/// Returns the index of the file at index, or of the largest video file
/// (falling back to the largest file) when index is None.
fn pick_file(files: &[TorrentFile], index: Option<usize>) -> anyhow::Result<usize> {
    if let Some(i) = index {
        if i >= files.len() {
            bail!(
                "file index {i} out of range (torrent has {} files)",
                files.len()
            );
        }
        return Ok(i);
    }
    let is_video = |f: &TorrentFile| content_type(&f.path).starts_with("video/");
    files
        .iter()
        .enumerate()
        .filter(|(_, f)| !f.padding)
        .max_by_key(|(_, f)| (is_video(f), f.len))
        .map(|(i, _)| i)
        .context("torrent has no files")
}

/// Opens the stream in IINA and returns once the player quits.
async fn launch_iina(url: String) -> anyhow::Result<()> {
    let bin = std::env::var_os("PATH")
        .and_then(|path| {
            std::env::split_paths(&path)
                .map(|d| d.join("iina"))
                .find(|p| p.is_file())
        })
        .or_else(|| {
            let app = PathBuf::from("/Applications/IINA.app/Contents/MacOS/iina-cli");
            app.is_file().then_some(app)
        })
        .context("IINA not found; install it with `brew install --cask iina`")?;
    let status = tokio::process::Command::new(bin)
        .args(["--no-stdin", "--keep-running", &url])
        .kill_on_drop(true)
        .status()
        .await
        .context("running IINA")?;
    if !status.success() {
        bail!("IINA {status}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_is_valid() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }

    #[test]
    fn parses_search_command_line() {
        // The command line search_interactive has fzf run.
        let cli = Cli::try_parse_from([
            "peerflix",
            "--print",
            "--user",
            "bob",
            "--trusted",
            "--source",
            "yts,eztv",
            "--",
            "-dash query",
        ])
        .unwrap();
        assert!(cli.print && cli.trusted);
        assert_eq!(cli.user.as_deref(), Some("bob"));
        assert_eq!(cli.sources, [Source::Yts, Source::Eztv]);
        assert_eq!(cli.source, ["-dash query"]);
    }

    #[test]
    fn picks_search_sources() {
        use Source::*;
        assert_eq!(search_sources(&[], false), [Nyaa, Yts, Eztv]);
        assert_eq!(search_sources(&[], true), [Nyaa]);
        assert_eq!(search_sources(&[Eztv, Yts, Eztv], true), [Eztv, Yts]);
    }

    #[test]
    fn nyaa_users() {
        for (s, want) in [
            ("", ""),
            ("someone", "someone"),
            ("https://nyaa.si/user/someone", "someone"),
            ("https://nyaa.si/user/someone/", "someone"),
            ("nyaa.si/user/someone", "someone"),
        ] {
            assert_eq!(nyaa_user(s), want, "{s:?}");
        }
    }

    #[test]
    fn torrent_sources() {
        let dir = tempfile::tempdir().unwrap();
        let existing = dir.path().join("movie.torrent");
        std::fs::write(&existing, "").unwrap();
        for (s, want) in [
            ("magnet:?xt=urn:btih:abc", true),
            ("http://example.com/a.torrent", true),
            ("https://example.com/a", true),
            (existing.to_str().unwrap(), true),
            ("missing.torrent", false),
            ("big buck bunny", false),
            ("", false),
            (dir.path().join("x.mkv").to_str().unwrap(), false),
        ] {
            assert_eq!(is_torrent_source(s), want, "{s:?}");
        }
    }

    fn files(spec: &[(&str, u64, bool)]) -> Vec<TorrentFile> {
        spec.iter()
            .map(|&(path, len, padding)| TorrentFile {
                path: path.into(),
                len,
                padding,
            })
            .collect()
    }

    #[test]
    fn picks_largest_video() {
        let fs = files(&[
            ("sample.mkv", 10, false),
            ("movie.MP4", 100, false),
            ("extras.zip", 1000, false),
            (".pad/5000", 5000, true),
            ("subs/en.srt", 4, false),
        ]);
        assert_eq!(pick_file(&fs, None).unwrap(), 1);
        assert_eq!(pick_file(&fs, Some(4)).unwrap(), 4);
        assert!(pick_file(&fs, Some(5)).is_err());
        assert!(pick_file(&[], None).is_err());
    }

    /// Seeds a torrent from local files with networking disabled and streams
    /// one of them through the HTTP server.
    #[tokio::test(flavor = "multi_thread")]
    async fn streams_local_torrent() {
        let dir = tempfile::tempdir().unwrap();
        let content = dir.path().join("content");
        std::fs::create_dir(&content).unwrap();
        let data = "0123456789".repeat(5000); // spans several pieces
        std::fs::write(content.join("video.mkv"), &data).unwrap();
        std::fs::write(content.join("other.txt"), "x").unwrap();

        let spawner = librqbit::spawn_utils::BlockingSpawner::new(1);
        let opts = librqbit::CreateTorrentOptions {
            piece_length: Some(16 << 10),
            ..Default::default()
        };
        let created = librqbit::create_torrent(&content, opts, &spawner)
            .await
            .unwrap();

        let session = Session::new_with_opts(
            dir.path().to_owned(),
            SessionOptions {
                dht: None,
                listen: None,
                disable_trackers: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let add = AddTorrent::from_bytes(created.as_bytes().unwrap());
        let meta = fetch_metadata(&session, add).await.unwrap();
        let files = torrent_files(&meta);
        let id = pick_file(&files, None).unwrap();
        assert_eq!(files[id].path, "video.mkv");

        let opts = AddTorrentOptions {
            only_files: Some(vec![id]),
            overwrite: true,
            ..Default::default()
        };
        let torrent = session
            .add_torrent(AddTorrent::from_bytes(meta.torrent_bytes), Some(opts))
            .await
            .unwrap()
            .into_handle()
            .unwrap();

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/video.mkv", listener.local_addr().unwrap());
        // librqbit's stream() waits for the initial check, which is too quick
        // here to overlap with the requests.
        let file = torrent_file(torrent.clone(), id, "video.mkv".into(), files[id].len);
        let _server = AbortOnDropHandle::new(tokio::spawn(stream::serve(listener, Arc::new(file))));

        let client = reqwest::Client::new();
        let resp = client.get(&url).send().await.unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.text().await.unwrap(), data);
        let resp = client
            .get(&url)
            .header("Range", "bytes=20000-20009")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 206);
        assert_eq!(resp.text().await.unwrap(), data[20000..20010]);
        session.stop().await;
    }

    #[test]
    fn falls_back_to_largest_file() {
        let fs = files(&[("small.txt", 1, false), ("big.bin", 50, false)]);
        assert_eq!(pick_file(&fs, None).unwrap(), 1);
    }
}
