use std::{
    collections::HashSet,
    io::Write,
    process::{Command, Stdio},
    time::Duration,
};

use anyhow::Context;
use tokio::task::JoinSet;

use crate::{eztv, nyaa, tpb, yts};

/// A site peerflix searches.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Source {
    /// nyaa.si, mostly anime
    Nyaa,
    /// YTS movies
    Yts,
    /// EZTV TV shows
    Eztv,
    /// The Pirate Bay's video torrents
    Tpb,
}

impl Source {
    pub const ALL: [Source; 4] = [Source::Nyaa, Source::Yts, Source::Eztv, Source::Tpb];

    pub fn name(self) -> &'static str {
        match self {
            Source::Nyaa => "nyaa",
            Source::Yts => "yts",
            Source::Eztv => "eztv",
            Source::Tpb => "tpb",
        }
    }
}

/// A search result from any source.
#[derive(Debug, Default)]
pub struct Torrent {
    /// A .torrent URL or magnet link.
    pub url: String,
    pub title: String,
    /// YYYY-MM-DD
    pub date: String,
    pub size: String,
    pub seeders: u32,
    pub leechers: u32,
}

/// The base URL each source is queried at.
#[derive(Clone, Debug)]
pub struct Endpoints {
    pub nyaa: String,
    pub yts: String,
    pub eztv: String,
    pub tpb: String,
    /// IMDb's title suggestions, which map EZTV queries to show IDs.
    pub imdb: String,
}

impl Endpoints {
    /// The public sites, each overridable by an environment variable, such as
    /// PEERFLIX_NYAA_URL pointing at a local mock for demos. Mirrors come and
    /// go, YTS's especially.
    pub fn from_env() -> Self {
        let var = |name, default: &str| std::env::var(name).unwrap_or_else(|_| default.to_owned());
        Self {
            nyaa: var("PEERFLIX_NYAA_URL", nyaa::NYAA_URL),
            yts: var("PEERFLIX_YTS_URL", yts::YTS_URL),
            eztv: var("PEERFLIX_EZTV_URL", eztv::EZTV_URL),
            tpb: var("PEERFLIX_TPB_URL", tpb::TPB_URL),
            imdb: var("PEERFLIX_IMDB_URL", eztv::IMDB_URL),
        }
    }
}

/// How long one source gets to answer before its results are dropped.
const TIMEOUT: Duration = Duration::from_secs(8);

/// The user quit the search without picking a torrent.
#[derive(Debug)]
pub struct NoSelection;

impl std::fmt::Display for NoSelection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("nothing selected")
    }
}

impl std::error::Error for NoSelection {}

/// Queries sources for query in parallel and writes their results as fzf
/// input as each source answers, one tab separated line per result: the
/// torrent URL, the date, size, health and source, and the title. A failed
/// search, such as a site being unavailable or rate limiting, prints nothing.
/// A magnet another source already listed is skipped. user and trusted only
/// apply to nyaa.
pub async fn print_results(
    w: &mut impl Write,
    endpoints: &Endpoints,
    sources: &[Source],
    query: &str,
    user: &str,
    trusted: bool,
) {
    let Ok(client) = reqwest::Client::builder().timeout(TIMEOUT).build() else {
        return;
    };
    let mut tasks = JoinSet::new();
    for &source in sources {
        let (client, ep) = (client.clone(), endpoints.clone());
        let (query, user) = (query.to_owned(), user.to_owned());
        tasks.spawn(async move {
            let items = match source {
                Source::Nyaa => nyaa::search(&client, &ep.nyaa, &query, &user, trusted).await,
                Source::Yts => yts::search(&client, &ep.yts, &query).await,
                Source::Eztv => eztv::search(&client, &ep.imdb, &ep.eztv, &query).await,
                Source::Tpb => tpb::search(&client, &ep.tpb, &query).await,
            };
            (source, items.unwrap_or_default())
        });
    }
    let mut seen = HashSet::new();
    while let Some(res) = tasks.join_next().await {
        let Ok((source, items)) = res else { continue };
        for it in items {
            if info_hash(&it.url).is_some_and(|h| !seen.insert(h)) {
                continue;
            }
            // A closed pipe just means fzf moved on to the next query.
            let _ = writeln!(
                w,
                "{}\t\x1b[90m{}  {:>10}\x1b[0m  {} \x1b[90m{:<4}\x1b[0m \t{}",
                it.url,
                it.date,
                it.size,
                health(&it),
                source.name(),
                it.title
            );
        }
        let _ = w.flush();
    }
}

/// Returns a magnet link's info hash in lowercase, or None for anything else.
fn info_hash(url: &str) -> Option<String> {
    let rest = url.strip_prefix("magnet:?")?;
    let start = rest.find("xt=urn:btih:")? + "xt=urn:btih:".len();
    let hash = rest[start..].split('&').next()?;
    Some(hash.to_ascii_lowercase())
}

/// Rates a torrent by its seeders relative to its leechers.
pub fn health(it: &Torrent) -> &'static str {
    use std::cmp::Ordering::*;
    match (it.seeders, it.seeders.cmp(&it.leechers)) {
        (0, _) => "\x1b[31m●\x1b[0m",
        (_, Greater) => "\x1b[32m●\x1b[0m",
        (_, Equal) => "\x1b[33m●\x1b[0m",
        (_, Less) => "\x1b[38;5;208m●\x1b[0m",
    }
}

/// Runs fzf over live searches of sources, with nyaa optionally restricted to
/// one uploader or to trusted uploads, and returns the chosen torrent URL or
/// magnet, or NoSelection if the user quits.
pub fn search_interactive(
    initial: &str,
    sources: &[Source],
    user: &str,
    trusted: bool,
) -> anyhow::Result<String> {
    let exe = std::env::current_exe()?;
    let names: Vec<_> = sources.iter().map(|s| s.name()).collect();
    let mut search = shell_quote(&exe.to_string_lossy()) + " --print --source " + &names.join(",");
    if !user.is_empty() {
        search += &format!(" --user {}", shell_quote(user));
    }
    if trusted {
        search += " --trusted";
    }
    search += " -- {q}";

    // fzf filters the current list on every keystroke, matching title terms
    // in the order the sources returned them, while a reload fetches the
    // results for the new query. fzf kills a running reload when the next one starts, so
    // the sleep debounces typing.
    let out = Command::new("fzf")
        .args(["--ansi", "--exact", "-i", "--no-sort", "--tabstop", "1"])
        .args(["--query", initial])
        .args(["--prompt", "search> "])
        .args(["--with-shell", "sh -c"])
        .args([
            "--delimiter",
            "\t",
            "--with-nth",
            "2..",
            "--nth",
            "2",
            "--accept-nth",
            "1",
        ])
        .args(["--bind", "enter:accept-non-empty"])
        .args(["--bind", &format!("start:reload:{search}")])
        .args(["--bind", &format!("change:reload:sleep 0.25; {search}")])
        .stdin(Stdio::inherit())
        .stderr(Stdio::inherit())
        .output()
        .context("running fzf (0.60 or later is required)")?;
    match out.status.code() {
        Some(0) => {}
        Some(1 | 130) => return Err(NoSelection.into()), // no match, or Esc/Ctrl-C
        _ => anyhow::bail!("running fzf (0.60 or later is required): {}", out.status),
    }
    let url = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    if url.is_empty() {
        return Err(NoSelection.into());
    }
    Ok(url)
}

pub fn human_bytes(n: u64) -> String {
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

/// The trackers put in magnets built from a bare info hash.
pub const TRACKERS: [&str; 5] = [
    "udp://tracker.opentrackr.org:1337/announce",
    "udp://open.stealth.si:80/announce",
    "udp://tracker.torrent.eu.org:451/announce",
    "udp://tracker.dler.org:6969/announce",
    "udp://open.dstud.io:6969/announce",
];

/// Builds a magnet link for an info hash, naming it name.
pub fn magnet(hash: &str, name: &str) -> String {
    let mut url =
        reqwest::Url::parse(&format!("magnet:?xt=urn:btih:{hash}")).expect("magnet URLs parse");
    let mut query = url.query_pairs_mut();
    query.append_pair("dn", name);
    for tr in TRACKERS {
        query.append_pair("tr", tr);
    }
    drop(query);
    url.into()
}

/// Formats a Unix time as a UTC YYYY-MM-DD date.
pub fn unix_date(secs: i64) -> String {
    // Howard Hinnant's civil_from_days.
    let z = secs.div_euclid(86_400) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

/// Quotes s for sh.
pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{nyaa::tests::SAMPLE_FEED, testutil::FakeServer, yts::tests::SAMPLE_JSON};

    fn endpoints(url: &str) -> Endpoints {
        Endpoints {
            nyaa: url.into(),
            yts: url.into(),
            eztv: url.into(),
            tpb: url.into(),
            imdb: url.into(),
        }
    }

    #[tokio::test]
    async fn prints_results() {
        let srv = FakeServer::start(200, SAMPLE_FEED).await;
        let mut buf = Vec::new();
        print_results(
            &mut buf,
            &endpoints(&srv.url),
            &[Source::Nyaa],
            "bunny",
            "",
            false,
        )
        .await;
        let out = String::from_utf8(buf).unwrap();
        let lines: Vec<_> = out.lines().collect();
        assert_eq!(lines.len(), 2, "{out}");
        let fields: Vec<_> = lines[0].split('\t').collect();
        assert_eq!(fields.len(), 3, "{:?}", lines[0]);
        assert_eq!(fields[0], "https://nyaa.si/download/1.torrent");
        assert!(
            fields[1].contains("2026-09-26")
                && fields[1].contains("1.2 GiB")
                && fields[1].contains("nyaa"),
            "{:?}",
            fields[1]
        );
        assert_eq!(fields[2], "[Group] Big Buck Bunny - 01 [1080p].mkv");
    }

    #[tokio::test]
    async fn failed_source_leaves_the_others() {
        // nyaa can't parse YTS's JSON, so only YTS's results are printed.
        let srv = FakeServer::start(200, SAMPLE_JSON).await;
        let mut buf = Vec::new();
        let sources = [Source::Nyaa, Source::Yts];
        print_results(&mut buf, &endpoints(&srv.url), &sources, "bunny", "", false).await;
        let out = String::from_utf8(buf).unwrap();
        assert_eq!(out.lines().count(), 2, "{out}");
        assert!(out.lines().all(|l| l.contains("yts")), "{out}");
    }

    #[tokio::test]
    async fn failed_search_prints_nothing() {
        let srv = FakeServer::start(503, "").await;
        let mut buf = Vec::new();
        print_results(
            &mut buf,
            &endpoints(&srv.url),
            &Source::ALL,
            "bunny",
            "",
            false,
        )
        .await;
        assert!(buf.is_empty());
    }

    #[tokio::test]
    async fn skips_repeated_magnets() {
        // YTS twice: the second copy's magnets are all repeats.
        let srv = FakeServer::start(200, SAMPLE_JSON).await;
        let mut buf = Vec::new();
        let sources = [Source::Yts, Source::Yts];
        print_results(&mut buf, &endpoints(&srv.url), &sources, "bunny", "", false).await;
        assert_eq!(String::from_utf8(buf).unwrap().lines().count(), 2);
    }

    #[test]
    fn info_hashes() {
        for (url, want) in [
            ("magnet:?xt=urn:btih:ABCdef&dn=x", Some("abcdef")),
            ("magnet:?dn=x&xt=urn:btih:abc", Some("abc")),
            ("magnet:?dn=x", None),
            ("https://nyaa.si/download/1.torrent", None),
        ] {
            assert_eq!(info_hash(url).as_deref(), want, "{url}");
        }
    }

    #[test]
    fn health_colors() {
        for (seeders, leechers, color) in [
            (0, 5, "\x1b[31m"),
            (0, 0, "\x1b[31m"),
            (10, 2, "\x1b[32m"),
            (3, 3, "\x1b[33m"),
            (1, 9, "\x1b[38;5;208m"),
        ] {
            let it = Torrent {
                seeders,
                leechers,
                ..Torrent::default()
            };
            assert!(
                health(&it).starts_with(color),
                "{seeders} seeders, {leechers} leechers"
            );
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

    #[test]
    fn unix_dates() {
        for (secs, want) in [
            (0, "1970-01-01"),
            (951_782_400, "2000-02-29"),
            (1_790_612_897, "2026-09-28"),
            (-86_400, "1969-12-31"),
        ] {
            assert_eq!(unix_date(secs), want, "{secs}");
        }
    }

    #[test]
    fn quotes_for_sh() {
        for (s, want) in [
            ("", "''"),
            ("plain", "'plain'"),
            ("/path with space", "'/path with space'"),
            ("it's", r"'it'\''s'"),
        ] {
            assert_eq!(shell_quote(s), want);
        }
    }
}
