//! peerflix streams a torrent (magnet link, .torrent file or URL) to IINA.
//!
//! The selected file is served over a local HTTP server with range support;
//! reads prioritize the pieces around the player's read position, so playback
//! starts as soon as the first pieces arrive and seeking works.

mod eztv;
mod nyaa;
mod search;
mod storage;
mod stream;
#[cfg(test)]
mod testutil;
mod tpb;
mod yts;

use std::{
    cmp::Ordering,
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
    search::{Category, Endpoints, NoSelection, human_bytes},
    storage::PartStorage,
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
/// searches for anime, movies or series interactively in fzf.
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

/// Raises the soft limit on open files as far as the system allows. librqbit
/// keeps every file of a torrent open, selected or not, so a big season pack
/// runs out of macOS's default of 256.
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
        source = search::search_interactive(&source, cli.category, user, cli.trusted)?;
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

    let Some(id) = select_file(cancel, &files, cli.index).await? else {
        return Ok(());
    };
    let file = &files[id];
    let name = Path::new(&file.path)
        .file_name()
        .map_or_else(|| file.path.clone(), |n| n.to_string_lossy().into_owned());

    // Download just that file and its subtitles, as .part files until
    // they're complete; streams still take priority. With --dir, existing
    // data is checked and reused.
    let subs = subtitles(&files, id);
    let wanted: Vec<usize> = [id].into_iter().chain(subs.iter().copied()).collect();
    let storage = PartStorage::new(meta.output_folder.clone());
    let opts = AddTorrentOptions {
        only_files: Some(wanted.clone()),
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
    let base = format!("http://{}", listener.local_addr()?);
    let url = format!("{base}/{}", path_escape(&name));
    let mut served = vec![torrent_file(torrent.clone(), id, name.clone(), file.len)];
    for &i in &subs {
        let sub_name = served_name(&files[i].path, &served);
        served.push(torrent_file(torrent.clone(), i, sub_name, files[i].len));
    }
    let sub_urls: Vec<_> = served[1..]
        .iter()
        .map(|f| format!("{base}/{}", path_escape(&f.name)))
        .collect();
    let sub_names: Vec<_> = served[1..].iter().map(|f| f.name.as_str()).collect();
    let sub_names = sub_names.join(", ");
    let _server = AbortOnDropHandle::new(tokio::spawn(stream::serve(listener, served.into())));

    eprintln!("Streaming {name} ({})\n{url}", human_bytes(file.len));
    if !sub_urls.is_empty() {
        eprintln!("Subtitles: {sub_names}");
    }

    let player = async {
        if cli.no_play {
            std::future::pending().await
        } else {
            launch_iina(url, &sub_urls).await
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
    let mut downloading = wanted;
    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                complete_files(&storage, &torrent.stats(), &files, &mut downloading);
                eprintln!();
                return Ok(());
            }
            result = &mut player => {
                complete_files(&storage, &torrent.stats(), &files, &mut downloading);
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
                complete_files(&storage, &stats, &files, &mut downloading);
                let (line, fetched) = progress_line(&stats, id, file.len, last_fetched);
                eprint!("\r\x1b[K{line}");
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
        "{:5.1}%  {:>10}/s  {live} peers, {seen} seen",
        100.0 * done as f64 / len.max(1) as f64,
        human_bytes(fetched.saturating_sub(last_fetched)),
    );
    (line, fetched)
}

/// Returns the file to stream: the one at index if given, else one the user
/// picks in fzf when the torrent holds several episodes and stdin is a
/// terminal, else the one pick_file chooses. None means peerflix was
/// cancelled meanwhile.
async fn select_file(
    cancel: &CancellationToken,
    files: &[TorrentFile],
    index: Option<usize>,
) -> anyhow::Result<Option<usize>> {
    let eps = episodes(files);
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
        .run_until_cancelled(search::choose("episode> ", lines))
        .await
    else {
        return Ok(None);
    };
    Ok(Some(choice?.parse().context("reading fzf's choice")?))
}

/// Returns the video files worth choosing between, episodes first, each group
/// in natural path order: those at least a tenth the size of the largest,
/// which leaves out samples.
fn episodes(files: &[TorrentFile]) -> Vec<usize> {
    let videos = || {
        files
            .iter()
            .enumerate()
            .filter(|(_, f)| !f.padding && is_video(f))
    };
    let largest = videos().map(|(_, f)| f.len).max().unwrap_or(0);
    let mut eps: Vec<usize> = videos()
        .filter(|(_, f)| f.len.saturating_mul(10) >= largest)
        .map(|(i, _)| i)
        .collect();
    // Episodes, tagged like S01E02, before extras such as featurettes.
    let untagged = |i: usize| episode_tag(&files[i].path.to_lowercase()).is_none();
    eps.sort_by(|&a, &b| {
        untagged(a)
            .cmp(&untagged(b))
            .then_with(|| natural_cmp(&files[a].path, &files[b].path))
    });
    eps
}

/// Compares strings with runs of digits compared as numbers, so Episode 2
/// sorts before Episode 10.
fn natural_cmp(mut a: &str, mut b: &str) -> Ordering {
    let digits = |s: &str| s.len() - s.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    loop {
        let (Some(x), Some(y)) = (a.chars().next(), b.chars().next()) else {
            return a.len().cmp(&b.len());
        };
        if x.is_ascii_digit() && y.is_ascii_digit() {
            let (na, ra) = a.split_at(digits(a));
            let (nb, rb) = b.split_at(digits(b));
            let (na, nb) = (na.trim_start_matches('0'), nb.trim_start_matches('0'));
            let ord = na.len().cmp(&nb.len()).then(na.cmp(nb));
            if ord != Ordering::Equal {
                return ord;
            }
            (a, b) = (ra, rb);
        } else if x != y {
            return x.cmp(&y);
        } else {
            (a, b) = (&a[x.len_utf8()..], &b[y.len_utf8()..]);
        }
    }
}

/// Returns the subtitle files that go with video file id: those named after
/// it (Show.S01E02.en.srt), in a folder named after it (Subs/Show.S01E02/),
/// or tagged with the same episode, or all of them when the torrent holds a
/// single video.
fn subtitles(files: &[TorrentFile], id: usize) -> Vec<usize> {
    let lower_stem = |p: &str| {
        Path::new(p)
            .file_stem()
            .map_or_else(String::new, |s| s.to_string_lossy().to_lowercase())
    };
    let video = &files[id].path;
    let stem = lower_stem(video);
    let tag = episode_tag(&stem);
    let single = episodes(files).len() <= 1;
    files
        .iter()
        .enumerate()
        .filter(|(_, f)| !f.padding && is_subtitle(f))
        .filter(|(_, f)| {
            let path = f.path.to_lowercase();
            let named = lower_stem(&path)
                .strip_prefix(&stem)
                .is_some_and(|rest| !rest.starts_with(|c: char| c.is_alphanumeric()));
            let in_dir = Path::new(&path)
                .parent()
                .is_some_and(|d| d.iter().any(|c| c.to_string_lossy() == stem));
            single
                || named
                || in_dir
                || tag.is_some_and(|t| episode_tag(&lower_stem(&path)) == Some(t))
        })
        .map(|(i, _)| i)
        .collect()
}

/// Returns the season and episode of the first S01E02 tag in the lowercase
/// string s.
fn episode_tag(s: &str) -> Option<(u32, u32)> {
    let digits = |s: &str| s.len() - s.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    s.match_indices('s').find_map(|(i, _)| {
        let rest = &s[i + 1..];
        let n = digits(rest);
        let season = rest[..n].parse().ok()?;
        let rest = rest[n..].strip_prefix('e')?;
        let episode = rest[..digits(rest)].parse().ok()?;
        Some((season, episode))
    })
}

fn is_subtitle(f: &TorrentFile) -> bool {
    let ext = Path::new(&f.path).extension().and_then(|e| e.to_str());
    ext.is_some_and(|e| ["srt", "ass", "ssa", "vtt"].contains(&e.to_ascii_lowercase().as_str()))
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

/// Joins paths into an mpv path list: colon separated, with a backslash
/// escaping a colon or backslash within a path.
fn mpv_path_list(paths: &[String]) -> String {
    let escaped: Vec<_> = paths
        .iter()
        .map(|p| p.replace('\\', "\\\\").replace(':', "\\:"))
        .collect();
    escaped.join(":")
}

fn is_video(f: &TorrentFile) -> bool {
    content_type(&f.path).starts_with("video/")
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
    files
        .iter()
        .enumerate()
        .filter(|(_, f)| !f.padding)
        .max_by_key(|(_, f)| (is_video(f), f.len))
        .map(|(i, _)| i)
        .context("torrent has no files")
}

/// Opens the stream in IINA with the subtitle URLs and returns once the player
/// quits.
async fn launch_iina(url: String, subs: &[String]) -> anyhow::Result<()> {
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
    let mut cmd = tokio::process::Command::new(bin);
    cmd.args(["--no-stdin", "--keep-running"]);
    if !subs.is_empty() {
        cmd.arg(format!("--mpv-sub-files={}", mpv_path_list(subs)));
    }
    let status = cmd
        .arg(&url)
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

    #[test]
    fn finds_episodes() {
        let fs = files(&[
            ("Show/Episode 10.mkv", 900, false),
            ("Show/Sample/sample.mkv", 50, false),
            ("Show/Episode 2.mkv", 1000, false),
            (".pad/100", 100, true),
            ("Show/Episode 1.mkv", 700, false),
            ("Show/cover.jpg", 5000, false),
        ]);
        assert_eq!(episodes(&fs), [4, 2, 0]);
        let pack = files(&[
            ("Featurettes/Making of.mkv", 900, false),
            ("Season 1/Show - S01E02.mkv", 1000, false),
            ("Season 1/Show - S01E01.mkv", 1000, false),
        ]);
        assert_eq!(episodes(&pack), [2, 1, 0]);
        let one = files(&[("movie.mkv", 1000, false), ("sample.mkv", 20, false)]);
        assert_eq!(episodes(&one), [0]);
    }

    #[test]
    fn finds_subtitles() {
        let fs = files(&[
            ("Show/Show.S01E01.mkv", 1000, false),
            ("Show/Show.S01E02.mkv", 1000, false),
            ("Show/Show.S01E01.en.srt", 5, false),
            ("Show/Show.S01E02.srt", 5, false),
            ("Show/Subs/Show.S01E01/2_English.srt", 5, false),
            ("Show/Subs/s01e01.French.ass", 5, false),
            ("Show/Show.S01E010.srt", 5, false),
            ("Show/notes.txt", 5, false),
        ]);
        assert_eq!(subtitles(&fs, 0), [2, 4, 5]);
        assert_eq!(subtitles(&fs, 1), [3]);

        let prefixes = files(&[
            ("Ep 1.mkv", 1000, false),
            ("Ep 10.mkv", 1000, false),
            ("Ep 1.srt", 5, false),
            ("Ep 10.srt", 5, false),
        ]);
        assert_eq!(subtitles(&prefixes, 0), [2]);

        let movie = files(&[
            ("Movie (2010)/Movie.mp4", 1000, false),
            ("Movie (2010)/Subs/English.srt", 5, false),
            ("Movie (2010)/Subs/Spanish.srt", 5, false),
        ]);
        assert_eq!(subtitles(&movie, 0), [1, 2]);
    }

    #[test]
    fn episode_tags() {
        for (s, want) in [
            ("show.s01e02.720p", Some((1, 2))),
            ("seasons.s1e10", Some((1, 10))),
            ("show s01", None),
            ("movie", None),
        ] {
            assert_eq!(episode_tag(s), want, "{s}");
        }
    }

    #[test]
    fn mpv_path_lists() {
        let urls = ["http://127.0.0.1:8888/a.srt".to_owned(), r"b\c".to_owned()];
        assert_eq!(mpv_path_list(&urls), r"http\://127.0.0.1\:8888/a.srt:b\\c");
    }

    #[test]
    fn natural_order() {
        use Ordering::*;
        for (a, b, want) in [
            ("ep2", "ep10", Less),
            ("S01E09", "S01E10", Less),
            ("S02E01", "S01E10", Greater),
            ("ep007", "ep7", Equal),
            ("a", "b", Less),
            ("ep1", "ep1.5", Less),
            ("", "", Equal),
        ] {
            assert_eq!(natural_cmp(a, b), want, "{a} vs {b}");
        }
    }

    #[test]
    fn falls_back_to_largest_file() {
        let fs = files(&[("small.txt", 1, false), ("big.bin", 50, false)]);
        assert_eq!(pick_file(&fs, None).unwrap(), 1);
    }
}
