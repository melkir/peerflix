//! peerflix streams a torrent (magnet link, .torrent file or URL) to IINA.
//!
//! The selected file is served over a local HTTP server with range support;
//! reads prioritize the pieces around the player's read position, so playback
//! starts as soon as the first pieces arrive and seeking works.

mod nyaa;
mod search;
mod stream;
#[cfg(test)]
mod testutil;

use std::{
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    process::ExitCode,
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, bail};
use clap::Parser;
use librqbit::{
    AddTorrent, AddTorrentOptions, AddTorrentResponse, DhtSessionConfig, ListenerOptions,
    ManagedTorrent, PeerConnectionOptions, Session, SessionOptions,
};
use tokio::{net::TcpListener, signal::unix::SignalKind};
use tokio_util::sync::CancellationToken;

use crate::{
    search::NoSelection,
    stream::{Reader, path_escape},
};

/// The release version, or the crate version for `cargo install` builds.
const VERSION: &str = match option_env!("PEERFLIX_VERSION") {
    Some(v) => v,
    None => env!("CARGO_PKG_VERSION"),
};

const VIDEO_EXTS: &[&str] = &[
    "mkv", "mp4", "avi", "mov", "webm", "m4v", "wmv", "flv", "ts", "m2ts", "mpg", "mpeg",
];

/// Stream a torrent straight into IINA.
///
/// With a magnet link, .torrent file or http(s) URL, streams it. Anything else
/// searches nyaa.si interactively in fzf.
#[derive(Parser, Debug)]
#[command(version = VERSION)]
struct Cli {
    /// Magnet link, .torrent file or URL, or nyaa search terms
    #[arg(value_name = "SOURCE | QUERY")]
    source: Vec<String>,

    /// HTTP port to serve the stream on (0 = random)
    #[arg(short, long, default_value_t = 8888)]
    port: u16,

    /// Download directory [default: temporary dir, removed on exit]
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

    /// Only search torrents from this nyaa uploader (name or profile URL)
    #[arg(short, long)]
    user: Option<String>,

    /// Only search torrents from trusted nyaa uploaders
    #[arg(short, long)]
    trusted: bool,

    /// Print nyaa results for the search terms as fzf input and exit
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
    // Don't wait on librqbit's blocking disk tasks; the data is disposable.
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
    let mut source = cli.source.join(" ");
    if cli.print {
        search::print_results(
            &mut std::io::stdout().lock(),
            nyaa::NYAA_URL,
            &source,
            user,
            cli.trusted,
        )
        .await;
        return Ok(());
    }
    if !is_torrent_source(&source) {
        source = search::search_interactive(&source, user, cli.trusted)?;
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
    // Declared first so it's removed after the session has closed its files.
    let tmp;
    let data_dir = match &cli.dir {
        Some(dir) => dir.clone(),
        None => {
            tmp = tempfile::Builder::new().prefix("peerflix-").tempdir()?;
            tmp.path().to_owned()
        }
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

/// Loads a magnet link, .torrent URL or .torrent file. librqbit can fetch
/// URLs and read files itself, but it takes any 40 character source for a
/// bare info hash, and nyaa's https://nyaa.si/download/NNNNNNN.torrent links
/// are exactly that long.
async fn load_torrent(source: &str) -> anyhow::Result<AddTorrent<'_>> {
    if source.starts_with("magnet:") {
        return Ok(AddTorrent::from_url(source));
    }
    if source.starts_with("http://") || source.starts_with("https://") {
        return Ok(AddTorrent::from_bytes(fetch_torrent(source).await?));
    }
    let data = tokio::fs::read(source)
        .await
        .with_context(|| format!("reading {source}"))?;
    Ok(AddTorrent::from_bytes(data))
}

async fn fetch_torrent(url: &str) -> anyhow::Result<bytes::Bytes> {
    let resp = reqwest::get(url).await.context("fetching torrent")?;
    if !resp.status().is_success() {
        bail!("fetching torrent: {}", resp.status());
    }
    resp.bytes().await.context("fetching torrent")
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
    // Resolve the metadata without adding the torrent, so nothing is
    // downloaded until a file is picked.
    let list_only = AddTorrentOptions {
        list_only: true,
        ..Default::default()
    };
    let Some(add) = cancel.run_until_cancelled(load_torrent(source)).await else {
        return Ok(());
    };
    let Some(resp) = cancel
        .run_until_cancelled(session.add_torrent(add?, Some(list_only)))
        .await
    else {
        return Ok(());
    };
    let AddTorrentResponse::ListOnly(meta) = resp? else {
        bail!("torrent was added instead of listed");
    };
    let files: Vec<_> = meta
        .info
        .iter_file_details()
        .map(|d| TorrentFile {
            path: d.filename.to_pathbuf().to_string_lossy().into_owned(),
            len: d.len,
            padding: d.attrs().padding,
        })
        .collect();

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

    // Download just that file; streams still take priority. With -dir,
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
    let server = tokio::spawn(stream::serve(listener, Arc::new(stream_file)));
    let _abort_server = AbortOnDrop(server);

    eprintln!("Streaming {name} ({})\n{url}", human_bytes(file.len));

    let mut player: Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send>> = if cli.no_play {
        Box::pin(std::future::pending())
    } else {
        Box::pin(launch_iina(url))
    };
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
                let stats = torrent.stats();
                let done = stats.file_progress.get(id).copied().unwrap_or(0);
                let (fetched, live, seen) = stats.live.as_ref().map_or((last_fetched, 0, 0), |l| {
                    let s = &l.snapshot;
                    (s.fetched_bytes, s.peer_stats.live, s.peer_stats.seen)
                });
                eprint!(
                    "\r\x1b[K{:5.1}%  {}/s  peers {live}/{seen}",
                    100.0 * done as f64 / file.len.max(1) as f64,
                    human_bytes(fetched.saturating_sub(last_fetched)),
                );
                last_fetched = fetched;
            }
        }
    }
}

/// Serves file id of torrent. With --dir, existing data is checked first; IINA
/// starts meanwhile and its first request waits for the check.
fn torrent_file(torrent: Arc<ManagedTorrent>, id: usize, name: String, len: u64) -> stream::File {
    stream::File {
        name,
        len,
        open: Box::new(move || {
            let t = torrent.clone();
            Box::pin(async move {
                t.wait_until_initialized().await?;
                Ok(Box::pin(t.stream(id).await?) as Reader)
            })
        }),
    }
}

struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
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
    let is_video = |f: &TorrentFile| {
        Path::new(&f.path)
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| VIDEO_EXTS.contains(&e.to_ascii_lowercase().as_str()))
    };
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

fn human_bytes(n: u64) -> String {
    const UNIT: u64 = 1024;
    if n < UNIT {
        return format!("{n} B");
    }
    let (mut div, mut exp) = (UNIT, 0);
    let mut m = n / UNIT;
    while m >= UNIT {
        div *= UNIT;
        exp += 1;
        m /= UNIT;
    }
    format!(
        "{:.1} {}iB",
        n as f64 / div as f64,
        "KMGTPE".as_bytes()[exp] as char
    )
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
            "--",
            "-dash query",
        ])
        .unwrap();
        assert!(cli.print && cli.trusted);
        assert_eq!(cli.user.as_deref(), Some("bob"));
        assert_eq!(cli.source, ["-dash query"]);
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

    #[test]
    fn human_sizes() {
        for (n, want) in [
            (0, "0 B"),
            (1023, "1023 B"),
            (1024, "1.0 KiB"),
            (1536, "1.5 KiB"),
            (1 << 20, "1.0 MiB"),
            (5 << 30, "5.0 GiB"),
            (3 << 40, "3.0 TiB"),
        ] {
            assert_eq!(human_bytes(n), want);
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
        let meta = match session
            .add_torrent(
                add,
                Some(AddTorrentOptions {
                    list_only: true,
                    ..Default::default()
                }),
            )
            .await
            .unwrap()
        {
            AddTorrentResponse::ListOnly(meta) => meta,
            _ => panic!("torrent was added instead of listed"),
        };
        let files: Vec<_> = meta
            .info
            .iter_file_details()
            .map(|d| TorrentFile {
                path: d.filename.to_pathbuf().to_string_lossy().into_owned(),
                len: d.len,
                padding: false,
            })
            .collect();
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
        // torrent_file waits for the initial check, which is too quick here
        // to overlap with the requests.
        let file = torrent_file(torrent.clone(), id, "video.mkv".into(), files[id].len);
        let _server = AbortOnDrop(tokio::spawn(stream::serve(listener, Arc::new(file))));

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

    #[tokio::test]
    async fn loads_40_char_urls_as_torrents() {
        let srv = testutil::FakeServer::start(200, "d4:infod4:name1:xee").await;
        // As long as an info hash, like nyaa's download links.
        let url = format!("{}/", srv.url);
        let url = format!("{url}{}", "x".repeat(40 - url.len()));
        assert_eq!(url.len(), 40);
        match load_torrent(&url).await.unwrap() {
            AddTorrent::TorrentFileBytes(b) => assert_eq!(&b[..], b"d4:infod4:name1:xee"),
            AddTorrent::Url(u) => panic!("loaded as URL {u:?}"),
        }

        let srv = testutil::FakeServer::start(404, "").await;
        let err = load_torrent(&format!("{}/a.torrent", srv.url))
            .await
            .err()
            .unwrap();
        assert!(err.to_string().contains("404"), "{err}");
        assert!(load_torrent("/nonexistent/a.torrent").await.is_err());
    }

    #[test]
    fn falls_back_to_largest_file() {
        let fs = files(&[("small.txt", 1, false), ("big.bin", 50, false)]);
        assert_eq!(pick_file(&fs, None).unwrap(), 1);
    }
}
