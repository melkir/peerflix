//! peerflix streams a torrent (magnet link, .torrent file or URL) to IINA,
//! or searches for one in fzf.

mod cli;

use std::{
    io::IsTerminal,
    path::{Path, PathBuf},
    process::ExitCode,
    sync::Arc,
    time::Duration,
};

use anyhow::Context;
use clap::Parser;
use librqbit::{AddTorrent, ManagedTorrent, Session};
use tokio::{
    net::TcpListener,
    signal::unix::SignalKind,
    sync::{mpsc, oneshot},
};
use tokio_util::sync::CancellationToken;

use peerflix::{
    search::{Category, Endpoints},
    stream::{Connections, Pick, Server},
    torrent::{
        self, Status, add_torrent, complete_files, fetch_metadata,
        files::{TorrentFile, episodes, pick_file, subtitles, torrent_files},
        serve_files,
        storage::PartStorage,
    },
    util::human_bytes,
};

use crate::cli::{
    fzf::{self, NoSelection},
    json, player,
};

/// The port the stream is served on unless --port says otherwise.
const DEFAULT_PORT: u16 = 8888;

/// How long a stream served with --json goes without a player connected
/// before peerflix exits.
const IDLE: Duration = Duration::from_secs(30);

/// How long peerflix waits with --json for a program to pick a file to
/// stream, in case the program went away.
const PICK_WAIT: Duration = Duration::from_secs(10 * 60);

/// The release version, or the crate version for `cargo install` builds.
const VERSION: &str = match option_env!("PEERFLIX_VERSION") {
    Some(v) => v,
    None => env!("CARGO_PKG_VERSION"),
};

/// Search for anime, movies and series, and stream the torrent into IINA.
///
/// Search terms (or none) open the search in fzf, where Tab switches
/// category. A magnet link, .torrent file or http(s) URL streams right away.
#[derive(Parser, Debug)]
#[command(version = VERSION)]
struct Cli {
    /// Search terms, or a magnet link, .torrent file or URL to stream
    #[arg(value_name = "QUERY | SOURCE")]
    source: Vec<String>,

    /// Category to start searching in
    #[arg(short, long, value_enum, default_value_t = Category::Anime, help_heading = "Search")]
    category: Category,

    /// Only search this nyaa uploader's anime (name or profile URL)
    #[arg(short, long, help_heading = "Search")]
    user: Option<String>,

    /// Only search trusted nyaa uploads
    #[arg(short, long, help_heading = "Search")]
    trusted: bool,

    /// File to stream, instead of picking the episode in fzf
    #[arg(short, long, help_heading = "Stream")]
    index: Option<usize>,

    /// Where to keep downloads, reused across runs [default: $TMPDIR/peerflix]
    #[arg(short, long, help_heading = "Stream")]
    dir: Option<PathBuf>,

    /// Local HTTP port to serve the stream on [default: 8888, or a free one if
    /// that's taken; 0 = random]
    #[arg(short, long, help_heading = "Stream")]
    port: Option<u16>,

    /// Only serve the stream, without launching IINA
    #[arg(short, long, help_heading = "Stream")]
    no_play: bool,

    /// Don't ask the router to forward the torrent port (UPnP)
    #[arg(long, help_heading = "Stream")]
    no_upnp: bool,

    /// Print JSON for programs, such as the IINA plugin (see the README)
    #[arg(long, conflicts_with = "print")]
    json: bool,

    /// Print results for the search terms as fzf input and exit; the live
    /// search reloads with it
    #[arg(long, hide = true)]
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
    if cli.json && !is_torrent_source(&source) {
        let providers = cli
            .category
            .providers(&Endpoints::from_env(), user, cli.trusted);
        return json::print_results(&mut std::io::stdout().lock(), &providers, &source).await;
    }
    if cli.print {
        let providers = cli
            .category
            .providers(&Endpoints::from_env(), user, cli.trusted);
        let status = fzf::print_results(
            &mut std::io::stdout().lock(),
            cli.category,
            &providers,
            &source,
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

    let session = torrent::session(data_dir, cancel.child_token(), !cli.no_upnp).await?;
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

    let eps = episodes(&files);
    let (server, picks) = Server::start(bind_listener(cli.port).await?, cancel.clone())?;
    let picked = if cli.json {
        let control = server.control_url();
        json::print_files(&mut std::io::stdout().lock(), &files, &eps, &control)?;
        wait_for_pick(cancel, picks, &files, cli.index).await
    } else {
        drop(picks);
        select_file(cancel, &files, &eps, cli.index)
            .await?
            .map(|id| (id, None))
    };
    let Some((id, reply)) = picked else {
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
    let torrent = match torrent {
        Ok(torrent) => torrent,
        Err(e) => {
            if let Some(reply) = reply {
                let _ = reply.send(Err(format!("{e:#}")));
            }
            return Err(e);
        }
    };

    let stream = serve_files(&server, &torrent, &files, id, &subs);
    eprintln!(
        "Streaming {} ({})\n{}",
        stream.name,
        human_bytes(files[id].len),
        stream.url
    );
    if !stream.sub_names.is_empty() {
        eprintln!("Subtitles: {}", stream.sub_names.join(", "));
    }
    if let Some(reply) = reply {
        let _ = reply.send(Ok(json::stream(&stream)));
    }

    let player = async {
        if cli.json {
            // The program that picked plays it; when it's done, no one is.
            idle(server.connections()).await;
            Ok(())
        } else if cli.no_play {
            std::future::pending().await
        } else {
            player::launch_iina(stream.url.clone(), &stream.sub_urls).await
        }
    };
    watch(cancel, player, &torrent, &storage, &files, id, wanted).await
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
) -> Option<(usize, Option<oneshot::Sender<Result<String, String>>>)> {
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
            Ok(id) => return Some((id, Some(pick.reply))),
            Err(e) => {
                let _ = pick.reply.send(Err(format!("{e:#}")));
            }
        }
    }
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
                let stats = torrent.stats();
                complete_files(storage, &stats, files, &mut downloading);
                if tty {
                    let status = Status::new(&stats, id, files[id].len);
                    eprint!("\r\x1b[K{}", progress_line(&status));
                }
            }
        }
    }
}

/// Returns once no player has been connected to the stream for IDLE, with
/// the time before the first one connects counting too.
async fn idle(connections: &Connections) {
    let mut ticker = tokio::time::interval(Duration::from_secs(1));
    let mut since = tokio::time::Instant::now();
    loop {
        ticker.tick().await;
        if connections.count() > 0 {
            since = tokio::time::Instant::now();
        } else if since.elapsed() >= IDLE {
            return;
        }
    }
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

/// Returns the line the terminal shows the stream's status on, with the same
/// numbers as the control's JSON.
fn progress_line(s: &Status) -> String {
    if s.checking {
        return "Checking existing data...".into();
    }
    format!(
        "{:5.1}%  {:>10}/s  {} peers, {} seen",
        100.0 * s.downloaded as f64 / s.size.max(1) as f64,
        human_bytes(s.download_speed),
        s.peers,
        s.seen,
    )
}

/// Returns the file to stream: the one at index if given, else one the user
/// picks in fzf among eps, the torrent's episodes, when there are several and
/// stdin is a terminal, else the one pick_file chooses. None means peerflix
/// was cancelled meanwhile.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugin_version_matches() {
        // cargo release bumps both; see release.toml.
        let info: serde_json::Value =
            serde_json::from_str(include_str!("../iina-plugin/Info.json")).unwrap();
        assert_eq!(info["version"], env!("CARGO_PKG_VERSION"));
    }

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
    fn progress_lines() {
        let mut status = Status {
            checking: true,
            downloaded: 50 << 20,
            size: 200 << 20,
            download_speed: 3 << 20,
            upload_speed: 0,
            peers: 12,
            seen: 40,
        };
        assert_eq!(progress_line(&status), "Checking existing data...");
        status.checking = false;
        assert_eq!(
            progress_line(&status),
            " 25.0%     3.0 MiB/s  12 peers, 40 seen"
        );
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
}
