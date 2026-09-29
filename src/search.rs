use std::{collections::HashSet, sync::Arc, time::Duration};

use tokio::task::JoinSet;

use crate::providers::{
    Provider, Query, Torrent,
    eztv::{self, Eztv},
    nyaa::{self, Nyaa},
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
    pub fn others(self) -> [Category; 2] {
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

/// The sites whose search failed.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Failed {
    /// The names of the sites that couldn't be reached or timed out.
    pub unanswered: Vec<&'static str>,
    /// What went wrong at the sites that answered with an error, such as a
    /// rate limit or an unknown nyaa user.
    pub errors: Vec<String>,
}

impl Failed {
    /// How many sites failed.
    pub fn count(&self) -> usize {
        self.unanswered.len() + self.errors.len()
    }
}

/// Searches providers for query in parallel, and as each one answers, passes
/// its name and results to found. Dead torrents, with no seeders, are left
/// out, and so is a torrent another site already listed, going by info hash.
///
/// Returns the sites whose search failed, such as by being unavailable or
/// rate limiting; they pass nothing to found.
pub async fn search(
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

#[cfg(test)]
mod tests {
    use futures_util::future::BoxFuture;

    use super::*;
    use crate::{providers::nyaa::tests::SAMPLE_FEED, testutil::FakeServer};

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
    async fn searches_category_sites() {
        let srv = FakeServer::start(200, SAMPLE_FEED).await;
        let providers = Category::Anime.providers(&endpoints(&srv.url), "", false);
        let (found, failed) = titles(&providers).await;
        // The sample's other torrent is dead.
        let want = vec!["[Group] Big Buck Bunny - 01 [1080p].mkv".to_owned()];
        assert_eq!(found, [("nyaa", want)]);
        assert_eq!(failed, Failed::default());
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
    async fn failed_search_finds_nothing() {
        let srv = FakeServer::start(503, "").await;
        for category in [Category::Anime, Category::Movies, Category::Series] {
            let providers = category.providers(&endpoints(&srv.url), "", false);
            let (found, failed) = titles(&providers).await;
            assert!(found.is_empty(), "{category:?}");
            assert_eq!(failed.count(), providers.len(), "{category:?}");
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
}
