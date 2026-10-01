//! peerflix streams a torrent (magnet link, .torrent file or URL) to IINA,
//! or searches for one in fzf.

mod cli;

use std::{io::IsTerminal, path::PathBuf, process::ExitCode, sync::Arc, time::Duration};

use anyhow::Context;
use clap::Parser;
use librqbit::Session;
use tokio::signal::unix::{Signal, SignalKind};
use tokio_util::sync::CancellationToken;

use peerflix_core::{
    http::{Server, bind_listener},
    play::{self, is_torrent_source},
    player::Iina,
    search::{self, Category, Endpoints},
    torrent,
    util::{self, human_bytes},
};

use crate::cli::{
    fzf::{self, NoSelection},
    json,
};

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

    /// Print JSON for programs, such as the IINA plugin (see docs/json.md)
    #[arg(long, conflicts_with = "print")]
    json: bool,

    /// Print results for the search terms as fzf input and exit; the live
    /// search reloads with it
    #[arg(long, hide = true)]
    print: bool,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    util::raise_open_file_limit();
    match run_on_tokio(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) if e.is::<NoSelection>() => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run_on_tokio(cli: Cli) -> anyhow::Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("starting the tokio runtime")?;
    let result = rt.block_on(async_main(cli));
    // Don't wait on librqbit's blocking disk tasks; the next run's data check
    // catches any piece they didn't finish writing.
    rt.shutdown_timeout(Duration::from_secs(1));
    result
}

async fn async_main(cli: Cli) -> anyhow::Result<()> {
    let user = nyaa_user(cli.user.as_deref().unwrap_or(""));
    let mut source = cli.source.join(" ");
    if cli.json && !is_torrent_source(&source) {
        let providers = cli
            .category
            .providers(&Endpoints::from_env(), user, cli.trusted);
        let client = search::client()?;
        let out = &mut std::io::stdout().lock();
        return json::print_results(out, &client, cli.category, &providers, &source).await;
    }
    if cli.print {
        let providers = cli
            .category
            .providers(&Endpoints::from_env(), user, cli.trusted);
        let status = fzf::print_results(
            &mut std::io::stdout().lock(),
            &search::client()?,
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
    let term = tokio::signal::unix::signal(SignalKind::terminate())
        .context("installing the SIGTERM handler")?;
    tokio::spawn(cancel_on_signal(term, cancel.clone()));
    run(&cancel, &source, &cli).await
}

/// Cancels cancel on Ctrl-C or term.
async fn cancel_on_signal(mut term: Signal, cancel: CancellationToken) {
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

async fn run(cancel: &CancellationToken, source: &str, cli: &Cli) -> anyhow::Result<()> {
    let data_dir = cli.dir.clone().unwrap_or_else(torrent::default_dir);

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
    let Some(listing) = cancel
        .run_until_cancelled(play::list(session, source))
        .await
    else {
        return Ok(());
    };
    let listing = listing?;
    let (server, picks) = Server::start(bind_listener(cli.port).await?, cancel.clone())?;
    if cli.json {
        return json::stream(cancel, session, listing, &server, picks, cli.index).await;
    }
    drop(picks);

    let (files, eps) = (&listing.files, &listing.episodes);
    let Some(id) = fzf::select_file(cancel, files, eps, cli.index).await? else {
        return Ok(());
    };
    let Some(playing) = cancel
        .run_until_cancelled(listing.play(session, &server, id))
        .await
    else {
        return Ok(());
    };
    let playing = playing?;
    let stream = &playing.stream;
    eprintln!(
        "Streaming {} ({})\n{}",
        stream.video.name,
        human_bytes(playing.size()),
        stream.video.url
    );
    if !stream.subtitles.is_empty() {
        let names: Vec<_> = stream.subtitles.iter().map(|s| s.name.as_str()).collect();
        eprintln!("Subtitles: {}", names.join(", "));
    }

    let url = stream.video.url.clone();
    let subs: Vec<_> = stream.subtitles.iter().map(|s| s.url.clone()).collect();
    let player = async {
        if cli.no_play {
            std::future::pending().await
        } else {
            Iina::open(&url, &subs)?.wait().await
        }
    };
    // Progress rewrites one line, which only suits a terminal.
    let tty = std::io::stderr().is_terminal();
    let result = playing
        .watch(cancel, player, |status| {
            if tty {
                eprint!("\r\x1b[K{}", status.describe());
            }
        })
        .await;
    if tty {
        eprintln!();
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugin_version_matches() {
        // cargo release bumps them all; see release.toml.
        let version = env!("CARGO_PKG_VERSION");
        let info: serde_json::Value =
            serde_json::from_str(include_str!("../../../iina-plugin/Info.json")).unwrap();
        assert_eq!(info["version"], version);
        let global = include_str!("../../../iina-plugin/global.js");
        assert!(global.contains(&format!("const VERSION = \"{version}\";")));
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
}
