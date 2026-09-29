use std::{collections::HashSet, io::Write, sync::Arc, time::Duration};

use tokio::task::JoinSet;

use crate::{
    eztv::{self, Eztv},
    nyaa::{self, Nyaa},
    provider::{Provider, Query, Torrent},
    tpb::{self, Tpb},
    yts::{self, Yts},
};

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

    /// The sites to search, at endpoints, with nyaa restricted to user's
    /// uploads when user isn't empty and to trusted uploads if trusted.
    pub fn providers(
        self,
        endpoints: &Endpoints,
        user: &str,
        trusted: bool,
    ) -> Vec<Arc<dyn Provider>> {
        let ep = endpoints.clone();
        match self {
            Category::Anime => vec![Arc::new(Nyaa {
                base: ep.nyaa,
                user: user.to_owned(),
                trusted,
            })],
            Category::Movies => vec![
                Arc::new(Yts { base: ep.yts }),
                Arc::new(Tpb {
                    base: ep.tpb,
                    kind: tpb::Kind::Movies,
                }),
            ],
            Category::Series => vec![
                Arc::new(Eztv {
                    base: ep.eztv,
                    imdb: ep.imdb,
                }),
                Arc::new(Tpb {
                    base: ep.tpb,
                    kind: tpb::Kind::Tv,
                }),
            ],
        }
    }
}

/// The base URL each site is queried at.
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

/// Searches providers, category's sites, for query in parallel and writes their
/// results as fzf input as each site answers, one tab separated line per
/// result: the torrent URL, the date, size, seeders (colored by health) and,
/// when the category has several sites, site, and the title.
///
/// Returns a status line for the search's header: which sites didn't answer,
/// or where else to look when nothing was found, or nothing when all went
/// well.
pub async fn print_results(
    w: &mut impl Write,
    category: Category,
    providers: &[Arc<dyn Provider>],
    query: &str,
) -> String {
    let mut printed = 0;
    let failed = search(providers, &Query::new(query), |site, items| {
        let site_column = if providers.len() > 1 {
            format!("  {site:<4}")
        } else {
            String::new()
        };
        for it in items {
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
    })
    .await;
    status(category, query, printed, &failed, providers.len())
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

/// Searches providers for query in parallel, and as each one answers, passes
/// its name and results to found. Dead torrents, with no seeders, are left
/// out, and so is a torrent another site already listed, going by info hash.
///
/// Returns the sites whose search failed, such as by being unavailable or
/// rate limiting; they pass nothing to found.
async fn search(
    providers: &[Arc<dyn Provider>],
    query: &Query,
    mut found: impl FnMut(&'static str, Vec<Torrent>),
) -> Failed {
    let mut failed = Failed::default();
    let Ok(client) = reqwest::Client::builder().timeout(TIMEOUT).build() else {
        failed.unanswered = providers.iter().map(|p| p.name()).collect();
        return failed;
    };
    let mut tasks = JoinSet::new();
    for (i, provider) in providers.iter().enumerate() {
        let (provider, client, query) = (provider.clone(), client.clone(), query.clone());
        tasks.spawn(async move {
            // The client's timeout bounds each request, and a site can make
            // several rounds of them, as EZTV does.
            // None if it timed out.
            let items = tokio::time::timeout(TIMEOUT, provider.search(&client, &query))
                .await
                .ok();
            (i, items)
        });
    }
    let mut seen = HashSet::new();
    let mut answered = Vec::new();
    while let Some(res) = tasks.join_next().await {
        let Ok((i, items)) = res else { continue };
        answered.push(i);
        let name = providers[i].name();
        let items = match items {
            Some(Ok(items)) => items,
            Some(Err(e)) if !unreachable(&e) => {
                failed.errors.push(e.to_string());
                continue;
            }
            _ => {
                failed.unanswered.push(name);
                continue;
            }
        };
        let live = items
            .into_iter()
            .filter(|it| it.seeders > 0)
            .filter(|it| it.info_hash.is_empty() || seen.insert(it.info_hash.clone()))
            .collect();
        found(name, live);
    }
    // A task that panicked never reported its site.
    for (i, p) in providers.iter().enumerate() {
        if !answered.contains(&i) {
            failed.unanswered.push(p.name());
        }
    }
    failed
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

#[cfg(test)]
mod tests {
    use futures_util::future::BoxFuture;

    use super::*;
    use crate::{nyaa::tests::SAMPLE_FEED, testutil::FakeServer};

    fn endpoints(url: &str) -> Endpoints {
        Endpoints {
            nyaa: url.into(),
            yts: url.into(),
            eztv: url.into(),
            tpb: url.into(),
            imdb: url.into(),
        }
    }

    /// A site that answers every search with the same results, as (info
    /// hash, seeders) pairs, or with error when it's set.
    struct FakeSite {
        name: &'static str,
        results: &'static [(&'static str, u32)],
        error: Option<&'static str>,
    }

    impl FakeSite {
        fn answering(
            name: &'static str,
            results: &'static [(&'static str, u32)],
        ) -> Arc<dyn Provider> {
            Arc::new(FakeSite {
                name,
                results,
                error: None,
            })
        }

        fn failing(name: &'static str, error: &'static str) -> Arc<dyn Provider> {
            Arc::new(FakeSite {
                name,
                results: &[],
                error: Some(error),
            })
        }
    }

    impl Provider for FakeSite {
        fn name(&self) -> &'static str {
            self.name
        }

        fn search<'a>(
            &'a self,
            _: &'a reqwest::Client,
            _: &'a Query,
        ) -> BoxFuture<'a, anyhow::Result<Vec<Torrent>>> {
            Box::pin(async move {
                if let Some(e) = self.error {
                    anyhow::bail!(e);
                }
                Ok(self
                    .results
                    .iter()
                    .map(|&(hash, seeders)| Torrent {
                        url: format!("magnet:?xt=urn:btih:{hash}"),
                        title: format!("{} {hash}", self.name),
                        info_hash: hash.into(),
                        seeders,
                        ..Torrent::default()
                    })
                    .collect())
            })
        }
    }

    /// Searches providers, returning each site's name and titles in the
    /// order they answered, and the sites that failed.
    async fn titles(providers: &[Arc<dyn Provider>]) -> (Vec<(&'static str, Vec<String>)>, Failed) {
        let mut found = Vec::new();
        let failed = search(providers, &Query::new("x"), |site, items| {
            found.push((site, items.into_iter().map(|it| it.title).collect()));
        })
        .await;
        (found, failed)
    }

    #[tokio::test]
    async fn prints_results() {
        let srv = FakeServer::start(200, SAMPLE_FEED).await;
        let mut buf = Vec::new();
        let providers = Category::Anime.providers(&endpoints(&srv.url), "", false);
        print_results(&mut buf, Category::Anime, &providers, "bunny").await;
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
    async fn prints_site_column() {
        let providers = [
            FakeSite::answering("yts", &[("aa", 5)]),
            FakeSite::failing("tpb", "searching tpb: 429 Too Many Requests"),
        ];
        let mut buf = Vec::new();
        let status = print_results(&mut buf, Category::Movies, &providers, "x").await;
        let out = String::from_utf8(buf).unwrap();
        assert_eq!(out.lines().count(), 1, "{out}");
        assert!(out.contains("  yts "), "{out}");
        assert_eq!(status, "searching tpb: 429 Too Many Requests.");
    }

    #[tokio::test]
    async fn failed_site_leaves_the_others() {
        let providers = [
            FakeSite::failing("nyaa", "parsing nyaa results"),
            FakeSite::answering("yts", &[("aa", 5)]),
        ];
        let (found, failed) = titles(&providers).await;
        assert_eq!(found, [("yts", vec!["yts aa".to_owned()])]);
        assert_eq!(failed.errors, ["parsing nyaa results"]);
        assert!(failed.unanswered.is_empty());
    }

    #[tokio::test]
    async fn failed_search_prints_nothing() {
        let srv = FakeServer::start(503, "").await;
        for category in [Category::Anime, Category::Movies, Category::Series] {
            let mut buf = Vec::new();
            let providers = category.providers(&endpoints(&srv.url), "", false);
            print_results(&mut buf, category, &providers, "bunny").await;
            assert!(buf.is_empty(), "{category:?}");
        }
    }

    #[tokio::test]
    async fn tells_errors_from_unreachable_sites() {
        let yts = |base: &str| -> [Arc<dyn Provider>; 1] { [Arc::new(Yts { base: base.into() })] };
        let srv = FakeServer::start(429, "").await;
        let (_, failed) = titles(&yts(&srv.url)).await;
        assert_eq!(failed.errors, ["searching yts: 429 Too Many Requests"]);
        assert!(failed.unanswered.is_empty());

        // Nothing listens on a port just freed.
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let (_, failed) = titles(&yts(&format!("http://127.0.0.1:{port}"))).await;
        assert_eq!(failed.unanswered, ["yts"]);
        assert!(failed.errors.is_empty());
    }

    #[tokio::test]
    async fn skips_dead_and_repeated_torrents() {
        let (found, _) = titles(&[FakeSite::answering(
            "yts",
            &[("aa", 5), ("bb", 0), ("", 3), ("", 2)],
        )])
        .await;
        // Torrents without a hash are never repeats.
        assert_eq!(found[0].1, ["yts aa", "yts ", "yts "]);

        // The second site's torrent was already listed.
        let providers = [
            FakeSite::answering("yts", &[("aa", 5)]),
            FakeSite::answering("tpb", &[("aa", 9), ("cc", 1)]),
        ];
        let (found, _) = titles(&providers).await;
        let all: Vec<_> = found.into_iter().flat_map(|(_, t)| t).collect();
        assert_eq!(all.len(), 2, "{all:?}");
        assert!(all.contains(&"tpb cc".to_owned()), "{all:?}");
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
}
