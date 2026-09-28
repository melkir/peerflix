use std::{
    collections::HashSet,
    io::Write,
    process::{Command, Stdio},
    time::Duration,
};

use anyhow::Context;
use tokio::{io::AsyncWriteExt, task::JoinSet};

use crate::{eztv, nyaa, tpb, yts};

/// What to search for, each from the sites that have it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Category {
    /// nyaa.si's anime
    Anime,
    /// YTS, and The Pirate Bay's movies
    Movies,
    /// EZTV, and The Pirate Bay's TV shows
    Series,
}

impl Category {
    pub fn name(self) -> &'static str {
        match self {
            Category::Anime => "anime",
            Category::Movies => "movies",
            Category::Series => "series",
        }
    }

    fn sites(self) -> &'static [Site] {
        match self {
            Category::Anime => &[Site::Nyaa],
            Category::Movies => &[Site::Yts, Site::Tpb(tpb::Kind::Movies)],
            Category::Series => &[Site::Eztv, Site::Tpb(tpb::Kind::Tv)],
        }
    }
}

/// A site peerflix searches.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Site {
    Nyaa,
    Yts,
    Eztv,
    Tpb(tpb::Kind),
}

impl Site {
    fn name(self) -> &'static str {
        match self {
            Site::Nyaa => "nyaa",
            Site::Yts => "yts",
            Site::Eztv => "eztv",
            Site::Tpb(_) => "tpb",
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
    /// The info hash in lowercase hex, or empty if the source doesn't give
    /// one.
    pub info_hash: String,
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

/// Queries category's sites for query in parallel and writes their results
/// as fzf input as each site answers, one tab separated line per result: the
/// torrent URL, the date, size, health and site, and the title. A failed
/// search, such as a site being unavailable or rate limiting, prints nothing.
/// A torrent another site already listed, going by info hash, is skipped.
/// user and trusted only apply to nyaa.
pub async fn print_results(
    w: &mut impl Write,
    endpoints: &Endpoints,
    category: Category,
    query: &str,
    user: &str,
    trusted: bool,
) {
    print_sites(w, endpoints, category.sites(), query, user, trusted).await;
}

async fn print_sites(
    w: &mut impl Write,
    endpoints: &Endpoints,
    sites: &[Site],
    query: &str,
    user: &str,
    trusted: bool,
) {
    let Ok(client) = reqwest::Client::builder().timeout(TIMEOUT).build() else {
        return;
    };
    let mut tasks = JoinSet::new();
    for &site in sites {
        let (client, ep) = (client.clone(), endpoints.clone());
        let (query, user) = (query.to_owned(), user.to_owned());
        tasks.spawn(async move {
            let items = match site {
                Site::Nyaa => nyaa::search(&client, &ep.nyaa, &query, &user, trusted).await,
                Site::Yts => yts::search(&client, &ep.yts, &query).await,
                Site::Eztv => eztv::search(&client, &ep.imdb, &ep.eztv, &query).await,
                Site::Tpb(kind) => tpb::search(&client, &ep.tpb, &query, kind).await,
            };
            (site, items.unwrap_or_default())
        });
    }
    let mut seen = HashSet::new();
    while let Some(res) = tasks.join_next().await {
        let Ok((site, items)) = res else { continue };
        for it in items {
            if !it.info_hash.is_empty() && !seen.insert(it.info_hash.clone()) {
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
                site.name(),
                it.title
            );
        }
        let _ = w.flush();
    }
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

/// Runs fzf over live searches, starting in category, with nyaa optionally
/// restricted to one uploader or to trusted uploads, and returns the chosen
/// torrent URL or magnet, or NoSelection if the user quits. Tab and
/// Shift-Tab switch category, keeping the query.
pub fn search_interactive(
    initial: &str,
    category: Category,
    user: &str,
    trusted: bool,
) -> anyhow::Result<String> {
    let exe = std::env::current_exe()?;
    // The prompt holds the category, as in "movies> ".
    let mut search =
        shell_quote(&exe.to_string_lossy()) + r#" --print --category "${FZF_PROMPT%> }""#;
    if !user.is_empty() {
        search += &format!(" --user {}", shell_quote(user));
    }
    if trusted {
        search += " --trusted";
    }
    search += " -- {q}";

    // fzf filters the current list on every keystroke, matching title terms
    // in the order the sites returned them, while a reload fetches the
    // results for the new query. fzf kills a running reload when the next
    // one starts, so the sleep debounces typing.
    //
    // Tab changes the prompt and reloads in one transform, since a reload
    // chained after change-prompt still sees the old prompt. The search
    // command comes from PEERFLIX_SEARCH so that fzf fills in its {q} at
    // reload time, quoted, rather than inside the transform's own command.
    let cycle = |order: [&str; 3]| {
        format!(
            r#"transform:case $FZF_PROMPT in {0}*) n={1};; {1}*) n={2};; *) n={0};; esac; echo "change-prompt($n> )+reload:$PEERFLIX_SEARCH""#,
            order[0], order[1], order[2]
        )
    };
    let out = Command::new("fzf")
        .env("PEERFLIX_SEARCH", &search)
        .args(["--ansi", "--exact", "-i", "--no-sort", "--tabstop", "1"])
        .args(["--query", initial])
        .args(["--prompt", &format!("{}> ", category.name())])
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
        .args([
            "--bind",
            &format!("tab:{}", cycle(["anime", "movies", "series"])),
        ])
        .args([
            "--bind",
            &format!("shift-tab:{}", cycle(["series", "movies", "anime"])),
        ])
        .args(["--bind", &format!("start:reload:{search}")])
        .args(["--bind", &format!("change:reload:sleep 0.25; {search}")])
        .stdin(Stdio::inherit())
        .stderr(Stdio::inherit())
        .output()
        .context(FZF)?;
    fzf_choice(out)
}

/// Runs fzf over lines, each the value to return, a tab, a dimmed detail
/// column, a tab and the text to match, and returns the chosen value, or
/// NoSelection if the user quits.
pub async fn choose(prompt: &str, lines: String) -> anyhow::Result<String> {
    let mut child = tokio::process::Command::new("fzf")
        .args(["--ansi", "--exact", "-i", "--no-sort", "--tabstop", "1"])
        .args(["--prompt", prompt])
        .args([
            "--delimiter",
            "\t",
            "--with-nth",
            "2..",
            // Counted after --with-nth hides the value.
            "--nth",
            "2",
            "--accept-nth",
            "1",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .context(FZF)?;
    let mut stdin = child.stdin.take().context(FZF)?;
    // fzf may quit before reading everything.
    let _ = stdin.write_all(lines.as_bytes()).await;
    drop(stdin);
    fzf_choice(child.wait_with_output().await.context(FZF)?)
}

const FZF: &str = "running fzf (0.60 or later is required)";

/// Returns what fzf printed for the accepted line, or NoSelection if the user
/// quit or nothing matched.
fn fzf_choice(out: std::process::Output) -> anyhow::Result<String> {
    match out.status.code() {
        Some(0) => {}
        Some(1 | 130) => return Err(NoSelection.into()), // no match, or Esc/Ctrl-C
        _ => anyhow::bail!("{FZF}: {}", out.status),
    }
    let choice = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    if choice.is_empty() {
        return Err(NoSelection.into());
    }
    Ok(choice)
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
            Category::Anime,
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
    async fn failed_site_leaves_the_others() {
        // nyaa can't parse YTS's JSON, so only YTS's results are printed.
        let srv = FakeServer::start(200, SAMPLE_JSON).await;
        let mut buf = Vec::new();
        let sites = [Site::Nyaa, Site::Yts];
        print_sites(&mut buf, &endpoints(&srv.url), &sites, "bunny", "", false).await;
        let out = String::from_utf8(buf).unwrap();
        assert_eq!(out.lines().count(), 2, "{out}");
        assert!(out.lines().all(|l| l.contains("yts")), "{out}");
    }

    #[tokio::test]
    async fn failed_search_prints_nothing() {
        let srv = FakeServer::start(503, "").await;
        for category in [Category::Anime, Category::Movies, Category::Series] {
            let mut buf = Vec::new();
            print_results(&mut buf, &endpoints(&srv.url), category, "bunny", "", false).await;
            assert!(buf.is_empty(), "{category:?}");
        }
    }

    #[tokio::test]
    async fn skips_repeated_magnets() {
        // YTS twice: the second copy's magnets are all repeats.
        let srv = FakeServer::start(200, SAMPLE_JSON).await;
        let mut buf = Vec::new();
        let sites = [Site::Yts, Site::Yts];
        print_sites(&mut buf, &endpoints(&srv.url), &sites, "bunny", "", false).await;
        assert_eq!(String::from_utf8(buf).unwrap().lines().count(), 2);
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
