use std::{collections::HashSet, io::Write, time::Duration};

use anyhow::{Context, bail};
use serde::de::DeserializeOwned;
use tokio::task::JoinSet;

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

    /// The other two categories, in Tab order.
    fn others(self) -> [Category; 2] {
        match self {
            Category::Anime => [Category::Movies, Category::Series],
            Category::Movies => [Category::Series, Category::Anime],
            Category::Series => [Category::Anime, Category::Movies],
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

/// How long one site gets to answer, all its requests included, before its
/// results are dropped.
const TIMEOUT: Duration = Duration::from_secs(8);

/// Queries category's sites for query in parallel and writes their results
/// as fzf input as each site answers, one tab separated line per result: the
/// torrent URL, the date, size, seeders (colored by health) and, when the
/// category has several sites, site, and the title. A failed search, such as
/// a site being unavailable or rate limiting, prints nothing. Dead torrents,
/// with no seeders, are left out, and so is a torrent another site already
/// listed, going by info hash. user and trusted only apply to nyaa.
///
/// Returns a status line for the search's header: which sites didn't answer,
/// or where else to look when nothing was found, or nothing when all went
/// well.
pub async fn print_results(
    w: &mut impl Write,
    endpoints: &Endpoints,
    category: Category,
    query: &str,
    user: &str,
    trusted: bool,
) -> String {
    let sites = category.sites();
    let (printed, failed) = print_sites(w, endpoints, sites, query, user, trusted).await;
    status(category, query, printed, &failed, sites.len())
}

/// The sites whose search failed.
#[derive(Debug, Default, PartialEq, Eq)]
struct Failed {
    /// The names of the sites that couldn't be reached or timed out.
    unanswered: Vec<&'static str>,
    /// What went wrong at the sites that answered with an error, such as a
    /// rate limit or an unknown nyaa user.
    errors: Vec<String>,
}

impl Failed {
    fn len(&self) -> usize {
        self.unanswered.len() + self.errors.len()
    }
}

/// Returns the number of results printed and the sites that failed.
async fn print_sites(
    w: &mut impl Write,
    endpoints: &Endpoints,
    sites: &[Site],
    query: &str,
    user: &str,
    trusted: bool,
) -> (usize, Failed) {
    let mut failed = Failed::default();
    let Ok(client) = reqwest::Client::builder().timeout(TIMEOUT).build() else {
        failed.unanswered = sites.iter().map(|s| s.name()).collect();
        return (0, failed);
    };
    let mut tasks = JoinSet::new();
    for &site in sites {
        let (client, ep) = (client.clone(), endpoints.clone());
        let (query, user) = (query.to_owned(), user.to_owned());
        tasks.spawn(async move {
            let search = async {
                match site {
                    Site::Nyaa => nyaa::search(&client, &ep.nyaa, &query, &user, trusted).await,
                    Site::Yts => yts::search(&client, &ep.yts, &query).await,
                    Site::Eztv => eztv::search(&client, &ep.imdb, &ep.eztv, &query).await,
                    Site::Tpb(kind) => tpb::search(&client, &ep.tpb, &query, kind).await,
                }
            };
            // The client's timeout bounds each request, and EZTV makes up to
            // three rounds of them.
            // None if it timed out.
            let items = tokio::time::timeout(TIMEOUT, search).await.ok();
            (site, items)
        });
    }
    let mut seen = HashSet::new();
    let (mut printed, mut answered) = (0, Vec::new());
    while let Some(res) = tasks.join_next().await {
        let Ok((site, items)) = res else { continue };
        answered.push(site);
        let items = match items {
            Some(Ok(items)) => items,
            Some(Err(e)) if !unreachable(&e) => {
                failed.errors.push(e.to_string());
                continue;
            }
            _ => {
                failed.unanswered.push(site.name());
                continue;
            }
        };
        let site_column = if sites.len() > 1 {
            format!("  {:<4}", site.name())
        } else {
            String::new()
        };
        for mut it in items {
            if it.seeders == 0 {
                continue;
            }
            let hash = std::mem::take(&mut it.info_hash);
            if !hash.is_empty() && !seen.insert(hash) {
                continue;
            }
            // A closed pipe just means fzf moved on to the next query.
            let _ = writeln!(
                w,
                "{}\t\x1b[90m{}  {:>10}\x1b[0m  {}{:>5}\x1b[90m{site_column}\x1b[0m \t{}",
                it.url,
                it.date,
                it.size,
                health(&it),
                it.seeders,
                it.title
            );
            printed += 1;
        }
        let _ = w.flush();
    }
    // A task that panicked never reported its site.
    for s in sites.iter().filter(|s| !answered.contains(s)) {
        failed.unanswered.push(s.name());
    }
    (printed, failed)
}

/// Whether e means the site couldn't be reached or took too long, rather
/// than answering with an error.
fn unreachable(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        c.downcast_ref::<reqwest::Error>()
            .is_some_and(|e| e.is_connect() || e.is_timeout())
    })
}

/// The header line for a search that printed printed results, with failed
/// holding the sites, out of sites, whose search failed.
fn status(
    category: Category,
    query: &str,
    printed: usize,
    failed: &Failed,
    sites: usize,
) -> String {
    let unanswered = failed.unanswered.join(" and ");
    if failed.unanswered.len() == sites {
        return format!("{unanswered} didn't answer. Check your connection and try again.");
    }
    let mut notes: Vec<String> = failed.errors.iter().map(|e| format!("{e}.")).collect();
    if !unanswered.is_empty() {
        notes.insert(0, format!("{unanswered} didn't answer."));
    }
    let missing = notes.join(" ");
    // No results says nothing when no site could search.
    if printed > 0 || failed.len() == sites {
        return missing;
    }
    let missing = if missing.is_empty() {
        missing
    } else {
        missing + " "
    };
    let [a, b] = category.others().map(Category::name);
    let query = query.trim();
    let none = if query.is_empty() {
        format!("No {} to show.", category.name())
    } else {
        format!("No {} results for \"{query}\".", category.name())
    };
    format!("{missing}{none} Tab searches {a} and {b}.")
}

/// Sends req to site and parses its JSON answer, failing unless the site
/// answered 200 OK.
pub async fn get_json<T: DeserializeOwned>(
    req: reqwest::RequestBuilder,
    site: &str,
) -> anyhow::Result<T> {
    let resp = req
        .send()
        .await
        .with_context(|| format!("searching {site}"))?;
    let status = resp.status();
    if status != reqwest::StatusCode::OK {
        bail!("searching {site}: {status}");
    }
    resp.json()
        .await
        .with_context(|| format!("parsing {site} results"))
}

/// Rates a live torrent by its seeders relative to its leechers, as the
/// color to print its seeder count in.
fn health(it: &Torrent) -> &'static str {
    use std::cmp::Ordering::*;
    match it.seeders.cmp(&it.leechers) {
        Greater => "\x1b[32m",
        Equal => "\x1b[33m",
        Less => "\x1b[38;5;208m",
    }
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
        // The sample's other torrent is dead.
        assert_eq!(lines.len(), 1, "{out}");
        let fields: Vec<_> = lines[0].split('\t').collect();
        assert_eq!(fields.len(), 3, "{:?}", lines[0]);
        assert_eq!(fields[0], "https://nyaa.si/download/1.torrent");
        assert!(
            fields[1].contains("2026-09-26")
                && fields[1].contains("1.2 GiB")
                // 42 seeders and 3 leechers: green.
                && fields[1].contains("\x1b[32m   42"),
            "{:?}",
            fields[1]
        );
        // Anime has one site, so no site column.
        assert!(!fields[1].contains("nyaa"), "{:?}", fields[1]);
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
        assert_eq!(out.lines().count(), 1, "{out}");
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
    async fn tells_errors_from_unreachable_sites() {
        let srv = FakeServer::start(429, "").await;
        let (_, failed) = print_sites(
            &mut Vec::new(),
            &endpoints(&srv.url),
            &[Site::Yts],
            "bunny",
            "",
            false,
        )
        .await;
        assert_eq!(failed.errors, ["searching yts: 429 Too Many Requests"]);
        assert!(failed.unanswered.is_empty());

        // Nothing listens on a port just freed.
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let (_, failed) = print_sites(
            &mut Vec::new(),
            &endpoints(&format!("http://127.0.0.1:{port}")),
            &[Site::Yts],
            "bunny",
            "",
            false,
        )
        .await;
        assert_eq!(failed.unanswered, ["yts"]);
        assert!(failed.errors.is_empty());
    }

    #[tokio::test]
    async fn skips_repeated_magnets() {
        // YTS twice: the second copy's magnets are all repeats.
        let srv = FakeServer::start(200, SAMPLE_JSON).await;
        let mut buf = Vec::new();
        let sites = [Site::Yts, Site::Yts];
        print_sites(&mut buf, &endpoints(&srv.url), &sites, "bunny", "", false).await;
        // One of YTS's two torrents in the sample is dead.
        assert_eq!(String::from_utf8(buf).unwrap().lines().count(), 1);
    }

    #[test]
    fn statuses() {
        use Category::*;
        let failed = |unanswered: &[&'static str], errors: &[&str]| Failed {
            unanswered: unanswered.to_vec(),
            errors: errors.iter().map(|e| e.to_string()).collect(),
        };
        let none = failed(&[], &[]);
        assert_eq!(status(Movies, "x", 5, &none, 2), "");
        assert_eq!(
            status(Movies, "x", 5, &failed(&["tpb"], &[]), 2),
            "tpb didn't answer."
        );
        assert_eq!(
            status(Movies, "x", 0, &failed(&["yts", "tpb"], &[]), 2),
            "yts and tpb didn't answer. Check your connection and try again."
        );
        assert_eq!(
            status(Anime, "sintel ", 0, &none, 1),
            "No anime results for \"sintel\". Tab searches movies and series."
        );
        assert_eq!(
            status(Series, "x", 0, &failed(&["tpb"], &[]), 2),
            "tpb didn't answer. No series results for \"x\". Tab searches anime and movies."
        );
        assert_eq!(
            status(Movies, "", 0, &none, 2),
            "No movies to show. Tab searches series and anime."
        );
        // A site's own error is shown as is, without blaming the connection.
        assert_eq!(
            status(
                Anime,
                "x",
                0,
                &failed(&[], &["nyaa user \"typo\" not found"]),
                1
            ),
            "nyaa user \"typo\" not found."
        );
        assert_eq!(
            status(
                Movies,
                "x",
                3,
                &failed(&["yts"], &["searching tpb: 429 Too Many Requests"]),
                2
            ),
            "yts didn't answer. searching tpb: 429 Too Many Requests."
        );
        assert_eq!(
            status(
                Movies,
                "x",
                0,
                &failed(&["yts"], &["parsing tpb results"]),
                2
            ),
            "yts didn't answer. parsing tpb results."
        );
    }

    #[test]
    fn health_colors() {
        for (seeders, leechers, color) in [
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
                health(&it) == color,
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
}
