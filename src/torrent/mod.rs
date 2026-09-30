//! Adding torrents to a librqbit session and serving their files.

pub mod files;
pub mod storage;

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, bail};
use librqbit::{
    AddTorrent, AddTorrentOptions, AddTorrentResponse, DhtSessionConfig, ListOnlyResponse,
    ListenerOptions, ManagedTorrent, PeerConnectionOptions, Session, SessionOptions, TorrentStats,
    TorrentStatsState, storage::StorageFactoryExt,
};
use serde::Serialize;
use tokio::net::TcpListener;
use tokio_util::{sync::CancellationToken, task::AbortOnDropHandle};

use crate::stream::{self, CONTROL_PATH, Connections, Control, Reader, path_escape};
use files::TorrentFile;
use storage::PartStorage;

/// Starts a torrent session that keeps its data in dir until cancel is
/// cancelled, asking the router to forward its port if upnp.
pub async fn session(
    dir: PathBuf,
    cancel: CancellationToken,
    upnp: bool,
) -> anyhow::Result<Arc<Session>> {
    Session::new_with_opts(
        dir,
        SessionOptions {
            cancellation_token: Some(cancel),
            // Don't leave DHT state behind in the user's cache dir.
            dht: Some(DhtSessionConfig {
                persistence: None,
                ..Default::default()
            }),
            listen: Some(ListenerOptions {
                enable_upnp_port_forwarding: upnp,
                ..Default::default()
            }),
            ..Default::default()
        },
    )
    .await
    .context("creating torrent session")
}

/// Resolves the torrent's metadata without adding it, so nothing is
/// downloaded until a file is picked.
pub async fn fetch_metadata(
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

/// Adds the torrent to download just the wanted files, as .part files until
/// they're complete; streams still take priority. Data already in the
/// session's directory is checked and reused.
pub async fn add_torrent(
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
pub struct Stream {
    /// The name file id is served under.
    pub name: String,
    pub url: String,
    /// The names and URLs of the subtitles served alongside.
    pub sub_names: Vec<String>,
    pub sub_urls: Vec<String>,
    /// Where to GET the stream's Status, or DELETE to stop streaming.
    pub control_url: String,
    /// The connections players have open to the stream.
    pub connections: Connections,
    _server: AbortOnDropHandle<()>,
}

/// How the streamed file's download is going.
#[derive(Serialize, Debug, PartialEq)]
pub struct Status {
    /// Whether the data already on disk is still being checked, which comes
    /// before downloading.
    pub checking: bool,
    /// Bytes of the file downloaded, of its size.
    pub downloaded: u64,
    pub size: u64,
    /// Bytes per second, for the whole torrent.
    pub download_speed: u64,
    pub upload_speed: u64,
    /// Peers connected, and seen in all.
    pub peers: u32,
    pub seen: u32,
}

impl Status {
    /// The status of file id, of size bytes, given the torrent's stats.
    pub fn new(stats: &TorrentStats, id: usize, size: u64) -> Status {
        let live = stats.live.as_ref();
        let peers = live.map(|l| &l.snapshot.peer_stats);
        Status {
            checking: matches!(stats.state, TorrentStatsState::Initializing { .. }),
            downloaded: stats.file_progress.get(id).copied().unwrap_or(0),
            size,
            download_speed: live.map_or(0, |l| l.download_speed.as_bytes()),
            upload_speed: live.map_or(0, |l| l.upload_speed.as_bytes()),
            peers: peers.map_or(0, |p| p.live),
            seen: peers.map_or(0, |p| p.seen),
        }
    }
}

/// Starts serving file id of torrent and its subtitles subs on listener,
/// with its Status and a way to cancel stop at the control URL.
pub fn serve_files(
    listener: TcpListener,
    torrent: &Arc<ManagedTorrent>,
    files: &[TorrentFile],
    id: usize,
    subs: &[usize],
    stop: CancellationToken,
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
    let control = control(torrent.clone(), id, files[id].len, stop);
    let connections = Connections::default();
    let url_of = |name: &str| format!("{base}/{}", path_escape(name));
    Ok(Stream {
        url: url_of(&name),
        sub_urls: served[1..].iter().map(|f| url_of(&f.name)).collect(),
        sub_names: served[1..].iter().map(|f| f.name.clone()).collect(),
        control_url: format!("{base}/{CONTROL_PATH}"),
        name,
        connections: connections.clone(),
        _server: AbortOnDropHandle::new(tokio::spawn(stream::serve(
            listener,
            served,
            control,
            connections,
        ))),
    })
}

/// The control of the stream of file id of torrent, of size bytes: its
/// Status, and cancelling stop to end it.
fn control(torrent: Arc<ManagedTorrent>, id: usize, size: u64, stop: CancellationToken) -> Control {
    Control {
        status: Box::new(move || {
            let status = Status::new(&torrent.stats(), id, size);
            serde_json::to_string(&status).expect("Status serializes")
        }),
        stop,
    }
}

/// Drops their .part suffix from the downloading files that have finished,
/// and removes them from downloading.
pub fn complete_files(
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

/// Serves file id of torrent. Existing data is checked first; the player
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
    use crate::torrent::files::{pick_file, torrent_files};

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
        let base = format!("http://{}", listener.local_addr().unwrap());
        let url = format!("{base}/video.mkv");
        // librqbit's stream() waits for the initial check, which is too quick
        // here to overlap with the requests.
        let file = torrent_file(torrent.clone(), id, "video.mkv".into(), files[id].len);
        let stop = CancellationToken::new();
        let _server = AbortOnDropHandle::new(tokio::spawn(stream::serve(
            listener,
            vec![file],
            control(torrent.clone(), id, files[id].len, stop.clone()),
            Connections::default(),
        )));

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

        let control = format!("{base}/{CONTROL_PATH}");
        let status: serde_json::Value = client
            .get(&control)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(status["checking"], false);
        assert_eq!(status["downloaded"], data.len());
        assert_eq!(status["size"], data.len());
        client.delete(&control).send().await.unwrap();
        assert!(stop.is_cancelled());
        session.stop().await;
    }
}
