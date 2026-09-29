//! peerflix streams a torrent (magnet link, .torrent file or URL) to IINA.
//!
//! The selected file is served over a local HTTP server with range support;
//! reads prioritize the pieces around the player's read position, so playback
//! starts as soon as the first pieces arrive and seeking works.

mod eztv;
mod files;
mod fzf;
mod nyaa;
mod player;
mod search;
mod storage;
mod stream;
#[cfg(test)]
mod testutil;
mod tpb;
mod util;
mod yts;

use std::{
    io::IsTerminal,
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
    storage::StorageFactoryExt,
};
use tokio::{net::TcpListener, signal::unix::SignalKind};
use tokio_util::{sync::CancellationToken, task::AbortOnDropHandle};

use crate::{
    files::{TorrentFile, episodes, pick_file, subtitles, torrent_files},
    fzf::NoSelection,
    search::{Category, Endpoints},
    storage::PartStorage,
    stream::{Reader, path_escape},
    util::human_bytes,
};

/// The port the stream is served on unless --port says otherwise.
const DEFAULT_PORT: u16 = 8888;

/// The release version, or the crate version for `cargo install` builds.
const VERSION: &str = match option_env!("PEERFLIX_VERSION") {
    Some(v) => v,
    None => env!("CARGO_PKG_VERSION"),
};

/// Stream a torrent straight into IINA.
///
/// With a magnet link, .torrent file or http(s) URL, streams it. Anything else
/// searches for anime, movies or series interactively in fzf.
#[derive(Parser, Debug)]
#[command(version = VERSION)]
struct Cli {
    /// Magnet link, .torrent file or URL, or search terms
    #[arg(value_name = "SOURCE | QUERY")]
    source: Vec<String>,

    /// HTTP port to serve the stream on (0 = random) [default: 8888, or a
    /// random one if that's taken]
    #[arg(short, long)]
    port: Option<u16>,

    /// Download directory, reused across runs [default: $TMPDIR/peerflix]
    #[arg(short, long)]
    dir: Option<PathBuf>,

    /// File index to stream [default: ask when there are several episodes,
    /// otherwise the largest video]
    #[arg(short, long)]
    index: Option<usize>,

    /// List files in the torrent and exit
    #[arg(short, long)]
    list: bool,

    /// Don't launch IINA, just serve the stream
    #[arg(short, long)]
    no_play: bool,

    /// Don't ask the router to forward the torrent port (UPnP)
    #[arg(long)]
    no_upnp: bool,

    /// What to search for; Tab switches between them in the search
    #[arg(short, long, value_enum, default_value_t = Category::Anime)]
    category: Category,

    /// Only search anime from this nyaa uploader (name or profile URL)
    #[arg(short, long)]
    user: Option<String>,

    /// Only search anime from trusted nyaa uploaders
    #[arg(short, long)]
    trusted: bool,

    /// Print results for the search terms as fzf input and exit
    #[arg(long)]
    print: bool,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    raise_open_file_limit();
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

/// Raises the soft limit on open files as far as the system allows. Peer
/// connections alone can come close to macOS's default of 256: streaming one
/// episode of a big season pack held 151 open.
fn raise_open_file_limit() {
    let mut lim = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: getrlimit and setrlimit only read and write lim.
    unsafe {
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) != 0 {
            return;
        }
        // macOS refuses more than kern.maxfilesperproc even when the hard
        // limit is unlimited, so fall back to OPEN_MAX, which it always takes.
        for want in [lim.rlim_max, 10240] {
            if want <= lim.rlim_cur {
                return;
            }
            let new = libc::rlimit {
                rlim_cur: want,
                rlim_max: lim.rlim_max,
            };
            if libc::setrlimit(libc::RLIMIT_NOFILE, &new) == 0 {
                return;
            }
        }
    }
}

async fn async_main(cli: Cli) -> anyhow::Result<()> {
    let user = nyaa_user(cli.user.as_deref().unwrap_or(""));
    let mut source = cli.source.join(" ");
    if cli.print {
        let status = search::print_results(
            &mut std::io::stdout().lock(),
            &Endpoints::from_env(),
            cli.category,
            &source,
            user,
            cli.trusted,
        )
        .await;
        // The interactive search shows it as its header.
        if let Some(path) = std::env::var_os("PEERFLIX_STATUS") {
            let _ = std::fs::write(path, status);
        }
        return Ok(());
    }
    if !is_torrent_source(&source) {
        source = fzf::search_interactive(&source, cli.category, user, cli.trusted)?;
    } else if !user.is_empty() || cli.trusted {
        eprintln!("warning: --user and --trusted only apply to searching anime; ignoring them");
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
                enable_upnp_port_forwarding: !cli.no_upnp,
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

    let eps = episodes(&files);
    let Some(id) = select_file(cancel, &files, &eps, cli.index).await? else {
        return Ok(());
    };
    let subs = subtitles(&files, id, eps.len() <= 1);
    let wanted: Vec<usize> = [id].into_iter().chain(subs.iter().copied()).collect();

    let storage = PartStorage::new(meta.output_folder.clone());
    let Some(torrent) = cancel
        .run_until_cancelled(add_torrent(session, meta, &storage, &wanted))
        .await
    else {
        return Ok(());
    };
    let torrent = torrent?;

    let listener = bind_listener(cli.port).await?;
    let stream = serve_files(listener, &torrent, &files, id, &subs)?;
    eprintln!(
        "Streaming {} ({})\n{}",
        stream.name,
        human_bytes(files[id].len),
        stream.url
    );
    if !stream.sub_names.is_empty() {
        eprintln!("Subtitles: {}", stream.sub_names.join(", "));
    }

    let player = async {
        if cli.no_play {
            std::future::pending().await
        } else {
            player::launch_iina(stream.url.clone(), &stream.sub_urls).await
        }
    };
    watch(cancel, player, &torrent, &storage, &files, id, wanted).await
}

/// Adds the torrent to download just the wanted files, as .part files until
/// they're complete; streams still take priority. With --dir, existing data
/// is checked and reused.
async fn add_torrent(
    session: &Arc<Session>,
    meta: ListOnlyResponse,
    storage: &PartStorage,
    wanted: &[usize],
) -> anyhow::Result<Arc<ManagedTorrent>> {
    let opts = AddTorrentOptions {
        only_files: Some(wanted.to_vec()),
        // Where librqbit would put it anyway, spelled out as storage uses it.
        output_folder: Some(meta.output_folder.to_string_lossy().into_owned()),
        storage_factory: Some(storage.clone().boxed()),
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
    session
        .add_torrent(AddTorrent::from_bytes(meta.torrent_bytes), Some(opts))
        .await?
        .into_handle()
        .context("torrent was not added")
}

/// The stream being served, until dropped.
struct Stream {
    /// The name file id is served under.
    name: String,
    url: String,
    /// The names and URLs of the subtitles served alongside.
    sub_names: Vec<String>,
    sub_urls: Vec<String>,
    _server: AbortOnDropHandle<()>,
}

/// Starts serving file id of torrent and its subtitles subs on listener.
fn serve_files(
    listener: TcpListener,
    torrent: &Arc<ManagedTorrent>,
    files: &[TorrentFile],
    id: usize,
    subs: &[usize],
) -> anyhow::Result<Stream> {
    let base = format!("http://{}", listener.local_addr()?);
    let name = served_name(&files[id].path, &[]);
    let mut served = vec![torrent_file(
        torrent.clone(),
        id,
        name.clone(),
        files[id].len,
    )];
    for &i in subs {
        let sub_name = served_name(&files[i].path, &served);
        served.push(torrent_file(torrent.clone(), i, sub_name, files[i].len));
    }
    let url_of = |name: &str| format!("{base}/{}", path_escape(name));
    Ok(Stream {
        url: url_of(&name),
        sub_urls: served[1..].iter().map(|f| url_of(&f.name)).collect(),
        sub_names: served[1..].iter().map(|f| f.name.clone()).collect(),
        name,
        _server: AbortOnDropHandle::new(tokio::spawn(stream::serve(listener, served.into()))),
    })
}

/// Shows progress and renames finished files until cancelled or the player
/// quits, and returns the player's result. downloading is the ids of the
/// files being downloaded, of which id is the one streamed.
async fn watch(
    cancel: &CancellationToken,
    player: impl Future<Output = anyhow::Result<()>>,
    torrent: &ManagedTorrent,
    storage: &PartStorage,
    files: &[TorrentFile],
    id: usize,
    mut downloading: Vec<usize>,
) -> anyhow::Result<()> {
    tokio::pin!(player);
    let mut ticker = tokio::time::interval_at(
        tokio::time::Instant::now() + Duration::from_secs(1),
        Duration::from_secs(1),
    );
    // Progress rewrites one line, which only suits a terminal.
    let tty = std::io::stderr().is_terminal();
    let mut last_fetched = 0;
    let mut init = torrent.wait_until_initialized();
    let mut initialized = false;
    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                complete_files(storage, &torrent.stats(), files, &mut downloading);
                if tty {
                    eprintln!();
                }
                return Ok(());
            }
            result = &mut player => {
                complete_files(storage, &torrent.stats(), files, &mut downloading);
                if tty {
                    eprintln!();
                }
                return result;
            }
            result = &mut init, if !initialized => {
                result.context("checking existing data")?;
                initialized = true;
            }
            _ = ticker.tick() => {
                if !initialized {
                    if tty {
                        eprint!("\r\x1b[KChecking existing data...");
                    }
                    continue;
                }
                let stats = torrent.stats();
                complete_files(storage, &stats, files, &mut downloading);
                let (line, fetched) = progress_line(&stats, id, files[id].len, last_fetched);
                if tty {
                    eprint!("\r\x1b[K{line}");
                }
                last_fetched = fetched;
            }
        }
    }
}

/// Drops their .part suffix from the downloading files that have finished,
/// and removes them from downloading.
fn complete_files(
    storage: &PartStorage,
    stats: &TorrentStats,
    files: &[TorrentFile],
    downloading: &mut Vec<usize>,
) {
    downloading.retain(|&i| {
        let done = stats.file_progress.get(i) == Some(&files[i].len);
        // A failed rename is retried on the next tick.
        !(done && storage.complete(i).is_ok())
    });
}

/// Binds the stream's port on localhost. Without an explicit port, that's
/// DEFAULT_PORT, or a random one when it's taken.
async fn bind_listener(port: Option<u16>) -> anyhow::Result<TcpListener> {
    let want = port.unwrap_or(DEFAULT_PORT);
    match TcpListener::bind(("127.0.0.1", want)).await {
        Ok(l) => Ok(l),
        Err(e) if port.is_none() && e.kind() == std::io::ErrorKind::AddrInUse => {
            Ok(TcpListener::bind(("127.0.0.1", 0)).await?)
        }
        Err(e) => Err(e).with_context(|| format!("listening on port {want}")),
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
        "{:5.1}%  {:>10}/s  {live} peers, {seen} seen",
        100.0 * done as f64 / len.max(1) as f64,
        human_bytes(fetched.saturating_sub(last_fetched)),
    );
    (line, fetched)
}

/// Returns the file to stream: the one at index if given, else one the user
/// picks in fzf among eps, the torrent's episodes, when there are several
/// and stdin is a terminal, else the one pick_file chooses. None means
/// peerflix was cancelled meanwhile.
async fn select_file(
    cancel: &CancellationToken,
    files: &[TorrentFile],
    eps: &[usize],
    index: Option<usize>,
) -> anyhow::Result<Option<usize>> {
    if index.is_some() || eps.len() < 2 || !std::io::stdin().is_terminal() {
        return pick_file(files, index).map(Some);
    }
    let lines: String = eps
        .iter()
        .map(|&i| {
            let f = &files[i];
            format!(
                "{i}\t\x1b[90m{:>9}\x1b[0m  \t{}\n",
                human_bytes(f.len),
                f.path
            )
        })
        .collect();
    let Some(choice) = cancel
        .run_until_cancelled(fzf::choose("episode> ", lines))
        .await
    else {
        return Ok(None);
    };
    Ok(Some(choice?.parse().context("reading fzf's choice")?))
}

/// Returns the file name to serve path under, prefixed with a number if one
/// of served already has it.
fn served_name(path: &str, served: &[stream::File]) -> String {
    let name = Path::new(path)
        .file_name()
        .map_or_else(|| path.to_owned(), |n| n.to_string_lossy().into_owned());
    let taken = |n: &str| served.iter().any(|f| f.name == n);
    if !taken(&name) {
        return name;
    }
    (2..)
        .map(|k| format!("{k}-{name}"))
        .find(|n| !taken(n))
        .expect("an unused name")
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
            "--category",
            "series",
            "--",
            "-dash query",
        ])
        .unwrap();
        assert!(cli.print && cli.trusted);
        assert_eq!(cli.user.as_deref(), Some("bob"));
        assert_eq!(cli.category, Category::Series);
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

        let storage = PartStorage::new(meta.output_folder.clone());
        let opts = AddTorrentOptions {
            only_files: Some(vec![id]),
            overwrite: true,
            output_folder: Some(meta.output_folder.to_string_lossy().into_owned()),
            storage_factory: Some(storage.boxed()),
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
        let _server =
            AbortOnDropHandle::new(tokio::spawn(stream::serve(listener, Arc::new([file]))));

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
}
