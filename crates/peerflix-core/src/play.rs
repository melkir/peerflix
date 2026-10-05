//! Streaming a torrent from start to end: listing its files, downloading the
//! one picked with its subtitles while serving them, and naming the files
//! that finish. The command line and the app both stream through it.

use std::{
    fs::{File, TryLockError},
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, anyhow, bail};
use futures_util::future::BoxFuture;
use librqbit::{
    AddTorrent, AddTorrentOptions, AddTorrentResponse, ListOnlyResponse, ManagedTorrent,
    PeerConnectionOptions, Session, TorrentStats, TorrentStatsState, api::TorrentIdOrHash,
    storage::StorageFactoryExt,
};
use serde::Serialize;
use tokio_util::sync::CancellationToken;

use crate::{
    http::{self, Download, Reader, Server},
    torrent::{
        Served, Status, Stream,
        files::{TorrentFile, episodes, subtitles, torrent_files},
        storage::PartStorage,
    },
};

/// Reports whether s is something librqbit can load rather than search terms:
/// a magnet link, an http(s) URL or a .torrent file.
pub fn is_torrent_source(s: &str) -> bool {
    ["magnet:", "http://", "https://"]
        .iter()
        .any(|p| s.starts_with(p))
        || (s.ends_with(".torrent") && Path::new(s).is_file())
}

/// A torrent's files, listed without downloading any.
pub struct Listing {
    meta: ListOnlyResponse,
    pub files: Vec<TorrentFile>,
    /// The ids of the episodes worth choosing between, in order.
    pub episodes: Vec<usize>,
}

/// Lists the files of the torrent at source, a magnet link, URL or .torrent
/// file.
pub async fn list(session: &Arc<Session>, source: &str) -> anyhow::Result<Listing> {
    let meta = fetch_metadata(session, AddTorrent::from_cli_argument(source)?).await?;
    let files = torrent_files(&meta);
    let episodes = episodes(&files);
    Ok(Listing {
        meta,
        files,
        episodes,
    })
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

impl Listing {
    /// The torrent's info hash, in hex.
    pub fn info_hash(&self) -> String {
        self.meta.info_hash.as_string()
    }

    /// Starts downloading file id and the subtitles that go with it, and
    /// serves them on server, until the Playing returned is done watching.
    /// The torrent can only play once at a time in a session, but the
    /// listing can play another of its files once that's done.
    pub async fn play(
        &self,
        session: &Arc<Session>,
        server: &Server,
        id: usize,
    ) -> anyhow::Result<Playing> {
        let Listing {
            meta,
            files,
            episodes,
        } = self;
        // librqbit hands back the torrent it has, with the files picked then
        // and the storage made then, so this pick would neither download nor
        // be named once complete.
        if session.get(TorrentIdOrHash::Hash(meta.info_hash)).is_some() {
            bail!("the torrent is already playing");
        }
        let lock = lock_torrent(&lock_path(&self.info_hash()), LOCK_WAIT).await?;
        let subs = subtitles(files, id, episodes.len() <= 1);
        let wanted: Vec<usize> = std::iter::once(id).chain(subs.iter().copied()).collect();
        let storage = PartStorage::new(meta.output_folder.clone());
        let torrent = add_torrent(session, meta, &storage, &wanted).await?;
        let control = Control {
            session: session.clone(),
            torrent,
            id,
            size: files[id].len,
        };
        let stream = serve_files(
            server,
            &control.torrent,
            files,
            id,
            &subs,
            Arc::new(control.clone()),
        );
        Ok(Playing {
            stream,
            control,
            storage,
            _lock: lock,
            downloading: wanted.iter().map(|&i| (i, files[i].len)).collect(),
        })
    }
}

/// Adds the torrent to download just the wanted files, as .part files until
/// they're complete; streams still take priority. Data already in the
/// session's directory is checked and reused.
async fn add_torrent(
    session: &Arc<Session>,
    meta: &ListOnlyResponse,
    storage: &PartStorage,
    wanted: &[usize],
) -> anyhow::Result<Arc<ManagedTorrent>> {
    let opts = AddTorrentOptions {
        only_files: Some(wanted.to_vec()),
        // Where librqbit would put it anyway, spelled out as storage uses it.
        output_folder: Some(meta.output_folder.to_string_lossy().into_owned()),
        storage_factory: Some(storage.clone().boxed()),
        overwrite: true,
        initial_peers: Some(meta.seen_peers.clone()),
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
        .add_torrent(
            AddTorrent::from_bytes(meta.torrent_bytes.clone()),
            Some(opts),
        )
        .await?
        .into_handle()
        .context("torrent was not added")
}

/// Starts streaming file id of torrent and its subtitles subs on server,
/// with download for its control.
fn serve_files(
    server: &Server,
    torrent: &Arc<ManagedTorrent>,
    files: &[TorrentFile],
    id: usize,
    subs: &[usize],
    download: Arc<dyn Download>,
) -> Stream {
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
    let at = |name: &str| Served {
        name: name.to_owned(),
        url: server.url(name),
    };
    let stream = Stream {
        video: at(&name),
        subtitles: served[1..].iter().map(|f| at(&f.name)).collect(),
    };
    server.stream(served, download);
    stream
}

/// Serves file id of torrent. Existing data is checked first; the player
/// starts meanwhile and librqbit holds its first request until the check ends.
fn torrent_file(torrent: Arc<ManagedTorrent>, id: usize, name: String, len: u64) -> http::File {
    http::File {
        name,
        len,
        open: Box::new(move || {
            let t = torrent.clone();
            Box::pin(async move { Ok(Box::pin(t.stream(id).await?) as Reader) })
        }),
    }
}

/// Returns the file name to serve path under, prefixed with a number if one
/// of served already has it.
fn served_name(path: &str, served: &[http::File]) -> String {
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

/// How long playing a torrent waits for another peerflix to stop playing
/// it, as one that was just told to stop does once it has let go of the
/// files.
const LOCK_WAIT: Duration = Duration::from_secs(10);

/// The file a peerflix playing the torrent with info_hash holds locked, so
/// that two never download the same files, racing each other. It's kept
/// regardless of where the files are, which only matters to two peerflix
/// playing the same torrent into different directories.
fn lock_path(info_hash: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("peerflix-{info_hash}.lock"))
}

/// Locks path, waiting up to wait for whoever holds it, and returns the file
/// that holds the lock until it's dropped, or closed as its process exits.
async fn lock_torrent(path: &Path, wait: Duration) -> anyhow::Result<File> {
    let file = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    let deadline = Instant::now() + wait;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(TryLockError::WouldBlock) if Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Err(TryLockError::WouldBlock) => bail!("the torrent is playing in another peerflix"),
            Err(TryLockError::Error(e)) => {
                return Err(e).with_context(|| format!("locking {}", path.display()));
            }
        }
    }
}

/// A file being downloaded and served with its subtitles.
pub struct Playing {
    pub stream: Stream,
    control: Control,
    storage: PartStorage,
    /// Held until the torrent is out of the session, done downloading.
    _lock: File,
    /// The files still downloading, by id and size, the one streamed among
    /// them.
    downloading: Vec<(usize, u64)>,
}

impl Playing {
    /// The size of the file streamed, in bytes.
    pub fn size(&self) -> u64 {
        self.control.size
    }

    /// Follows and pauses the download, for as long as it's watched.
    pub fn control(&self) -> Control {
        self.control.clone()
    }

    /// Keeps downloading, passing the status to on_status every second and
    /// renaming the files that finish, until cancelled, player returns or the
    /// download fails, and returns the player's result or the download's
    /// error. The player is then dropped, and the torrent removed from the
    /// session, keeping its files, so it stops downloading and can play
    /// again.
    pub async fn watch(
        mut self,
        cancel: &CancellationToken,
        player: impl Future<Output = anyhow::Result<()>>,
        mut on_status: impl FnMut(&Status),
    ) -> anyhow::Result<()> {
        let mut ticker = tokio::time::interval_at(
            tokio::time::Instant::now() + Duration::from_secs(1),
            Duration::from_secs(1),
        );
        let torrent = self.control.torrent.clone();
        let mut init = torrent.wait_until_initialized();
        let mut initialized = false;
        // The player is pinned in here, so it's dropped, closing it, as soon
        // as watching stops rather than once the torrent is removed, which
        // can outlast the time an app gets to quit.
        let result = {
            tokio::pin!(player);
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break Ok(()),
                    result = &mut player => break result,
                    result = &mut init, if !initialized => {
                        if let Err(e) = result {
                            break Err(e.context("checking existing data"));
                        }
                        initialized = true;
                    }
                    _ = ticker.tick() => {
                        let stats = self.complete_files();
                        if let TorrentStatsState::Error = stats.state {
                            let error = stats.error.as_deref().unwrap_or("unknown error");
                            break Err(anyhow!("downloading: {error}"));
                        }
                        on_status(&Status::new(&stats, self.control.id, self.control.size));
                    }
                }
            }
        };
        self.complete_files();
        let removed = self
            .control
            .session
            .delete(TorrentIdOrHash::Id(torrent.id()), false)
            .await;
        result.and(removed.context("removing the torrent"))
    }

    /// Drops their .part suffix from the files that have finished, removing
    /// them from downloading, and returns the torrent's stats they were told
    /// by.
    fn complete_files(&mut self) -> TorrentStats {
        let stats = self.control.torrent.stats();
        self.downloading.retain(|&(i, len)| {
            let done = stats.file_progress.get(i) == Some(&len);
            // A failed rename is retried on the next tick.
            !(done && self.storage.complete(i).is_ok())
        });
        stats
    }
}

/// Follows and pauses the download of a file being watched.
#[derive(Clone)]
pub struct Control {
    session: Arc<Session>,
    torrent: Arc<ManagedTorrent>,
    /// The file streamed, and its size.
    id: usize,
    size: u64,
}

impl Control {
    /// How the download is going.
    pub fn status(&self) -> Status {
        Status::new(&self.torrent.stats(), self.id, self.size)
    }

    /// Stops downloading until unpaused, with paused, or resumes it; the
    /// player waits meanwhile. Pausing it twice fails.
    pub async fn set_paused(&self, paused: bool) -> anyhow::Result<()> {
        if paused {
            self.session.pause(&self.torrent).await
        } else {
            self.session.unpause(&self.torrent).await
        }
    }
}

impl Download for Control {
    /// The status, with whether it can be paused and the line describing it
    /// as text, so that programs needn't work them out.
    fn status_json(&self) -> String {
        #[derive(Serialize)]
        struct Json {
            #[serde(flatten)]
            status: Status,
            pausable: bool,
            text: String,
        }
        let status = self.status();
        let pausable = status.pausable();
        let text = status.describe();
        serde_json::to_string(&Json {
            status,
            pausable,
            text,
        })
        .expect("the status serializes")
    }

    fn set_paused(&self, paused: bool) -> BoxFuture<'static, anyhow::Result<()>> {
        let control = self.clone();
        Box::pin(async move { Control::set_paused(&control, paused).await })
    }
}

#[cfg(test)]
mod tests {
    use librqbit::{SessionOptions, TorrentStatsState};
    use tokio::net::TcpListener;

    use super::*;
    use crate::torrent::State;

    /// Lists a .torrent file whose data is already on disk, plays its video
    /// and watches the download, with networking disabled.
    #[tokio::test(flavor = "multi_thread")]
    async fn plays_local_torrent() {
        let dir = tempfile::tempdir().unwrap();
        let content = dir.path().join("content");
        std::fs::create_dir(&content).unwrap();
        let data = "0123456789".repeat(5000); // spans several pieces
        std::fs::write(content.join("video.mkv"), &data).unwrap();
        std::fs::write(content.join("video.srt"), "1").unwrap();
        let spawner = librqbit::spawn_utils::BlockingSpawner::new(1);
        let opts = librqbit::CreateTorrentOptions {
            piece_length: Some(16 << 10),
            ..Default::default()
        };
        let created = librqbit::create_torrent(&content, opts, &spawner)
            .await
            .unwrap();
        let file = dir.path().join("content.torrent");
        std::fs::write(&file, created.as_bytes().unwrap()).unwrap();
        let source = file.to_str().unwrap();
        assert!(is_torrent_source(source));

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
        let listing = list(&session, source).await.unwrap();
        let id = listing.episodes[0];
        assert_eq!(listing.files[id].path, "video.mkv");

        let stop = CancellationToken::new();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (server, _) = Server::start(listener, stop.clone()).unwrap();
        let playing = listing.play(&session, &server, id).await.unwrap();
        assert_eq!(playing.size(), data.len() as u64);
        let sub_url = server.url("video.srt");
        assert_eq!(
            playing.stream.subtitles,
            [Served {
                name: "video.srt".into(),
                url: sub_url
            }]
        );

        // Served whole and in ranges.
        let client = reqwest::Client::new();
        let get = |range: Option<&str>| {
            let req = client.get(&playing.stream.video.url);
            match range {
                Some(r) => req.header("Range", r).send(),
                None => req.send(),
            }
        };
        let resp = get(None).await.unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.text().await.unwrap(), data);
        let resp = get(Some("bytes=20000-20009")).await.unwrap();
        assert_eq!(resp.status(), 206);
        assert_eq!(resp.text().await.unwrap(), data[20000..20010]);

        // The control tells the status.
        let control = server.control_url();
        let status: serde_json::Value = client
            .get(&control)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(status["state"], "done");
        assert_eq!(status["pausable"], false);
        assert_eq!(status["downloaded"], data.len());
        assert_eq!(status["text"], "100.0%  downloaded");

        // It plays once at a time.
        let err = listing.play(&session, &server, id).await.err().unwrap();
        assert!(err.to_string().contains("already playing"), "{err:#}");

        // Paused from the control too; with the data all there, the status
        // says it's done rather than paused.
        let paused = || {
            matches!(
                playing.control.torrent.stats().state,
                TorrentStatsState::Paused
            )
        };
        playing.control().set_paused(true).await.unwrap();
        assert!(paused());
        assert_eq!(playing.control().status().state, State::Done);
        let pause = |q: &str| client.put(format!("{control}?{q}")).send();
        assert_eq!(pause("pause").await.unwrap().status(), 409);
        assert_eq!(pause("resume").await.unwrap().status(), 204);
        assert!(!paused());

        let mut statuses = Vec::new();
        let player = async {
            tokio::time::sleep(Duration::from_millis(1500)).await;
            Ok(())
        };
        playing
            .watch(&stop, player, |s| statuses.push(s.downloaded))
            .await
            .unwrap();
        assert_eq!(statuses, [data.len() as u64]);

        // Done watching, it's out of the session, so the same listing can
        // play again.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (server, _) = Server::start(listener, stop.clone()).unwrap();
        listing.play(&session, &server, id).await.unwrap();

        client.delete(server.control_url()).send().await.unwrap();
        assert!(stop.is_cancelled());
        session.stop().await;
    }

    #[tokio::test]
    async fn locks_a_torrent_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hash.lock");
        let wait = Duration::from_millis(300);
        let held = lock_torrent(&path, wait).await.unwrap();
        let err = lock_torrent(&path, wait).await.unwrap_err();
        assert!(err.to_string().contains("another peerflix"), "{err:#}");

        // Let go of while waiting, as by a peerflix told to stop.
        let waiting = tokio::spawn(async move { lock_torrent(&path, wait).await });
        tokio::time::sleep(Duration::from_millis(100)).await;
        drop(held);
        waiting.await.unwrap().unwrap();
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
