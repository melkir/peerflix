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
use tokio_util::sync::CancellationToken;

use crate::{
    http::{self, Download, Reader, Server},
    util::human_bytes,
};
use files::TorrentFile;
use storage::PartStorage;

/// Where downloads are kept unless told otherwise, reused across runs so
/// playing a torrent again resumes it. macOS clears $TMPDIR of files that go
/// unused for a few days.
pub fn default_dir() -> PathBuf {
    std::env::temp_dir().join("peerflix")
}

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
pub(crate) async fn fetch_metadata(
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
pub(crate) async fn add_torrent(
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

/// A file served, under its name at its URL.
#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct Served {
    pub name: String,
    pub url: String,
}

/// The file being streamed and the subtitles served alongside, as JSON the
/// video's name and url, and its subtitles.
#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct Stream {
    #[serde(flatten)]
    pub video: Served,
    pub subtitles: Vec<Served>,
}

/// Where the streamed file's download is at.
#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum State {
    /// The data already on disk is being checked, which comes first.
    Checking,
    Downloading,
    Paused,
    /// The file is all there.
    Done,
}

/// How the streamed file's download is going.
#[derive(Serialize, Clone, Copy, Debug, PartialEq)]
pub struct Status {
    pub state: State,
    /// Bytes of the file downloaded, of its size.
    pub downloaded: u64,
    pub size: u64,
    /// Bytes per second, for the whole torrent.
    pub download_speed: u64,
    /// Peers connected, and seen in all.
    pub peers: u32,
    pub seen: u32,
}

impl Status {
    /// The status of file id, of size bytes, given the torrent's stats.
    pub fn new(stats: &TorrentStats, id: usize, size: u64) -> Status {
        let live = stats.live.as_ref();
        let peers = live.map(|l| &l.snapshot.peer_stats);
        let downloaded = stats.file_progress.get(id).copied().unwrap_or(0);
        let state = match stats.state {
            TorrentStatsState::Initializing { .. } => State::Checking,
            _ if downloaded >= size => State::Done,
            TorrentStatsState::Paused => State::Paused,
            _ => State::Downloading,
        };
        Status {
            state,
            downloaded,
            size,
            download_speed: live.map_or(0, |l| l.download_speed.as_bytes()),
            peers: peers.map_or(0, |p| p.live),
            seen: peers.map_or(0, |p| p.seen),
        }
    }

    /// How much of the file is downloaded, in percent.
    pub fn percent(&self) -> f64 {
        100.0 * self.downloaded as f64 / self.size.max(1) as f64
    }

    /// Whether the download can be paused or resumed: not while the data is
    /// checked, nor once it's all there.
    pub fn pausable(&self) -> bool {
        matches!(self.state, State::Downloading | State::Paused)
    }

    /// The status in a line, as the command line, the app and the IINA plugin
    /// show it, its numbers in columns of their own width so a line rewritten
    /// in place stays put.
    pub fn describe(&self) -> String {
        let percent = self.percent();
        match self.state {
            State::Checking => "Checking existing data...".into(),
            State::Done => format!("{percent:5.1}%  downloaded"),
            State::Paused => format!("{percent:5.1}%  paused"),
            State::Downloading => format!(
                "{percent:5.1}%  {:>10}/s  {} peers, {} seen",
                human_bytes(self.download_speed),
                self.peers,
                self.seen,
            ),
        }
    }
}

/// Starts streaming file id of torrent and its subtitles subs on server,
/// with download for its control.
pub(crate) fn serve_files(
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

/// Drops their .part suffix from the downloading files that have finished,
/// and removes them from downloading.
pub(crate) fn complete_files(
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describes_statuses() {
        let mut status = Status {
            state: State::Checking,
            downloaded: 50 << 20,
            size: 200 << 20,
            download_speed: 3 << 20,
            peers: 12,
            seen: 40,
        };
        assert_eq!(status.describe(), "Checking existing data...");
        assert!(!status.pausable());
        status.state = State::Downloading;
        assert_eq!(status.describe(), " 25.0%     3.0 MiB/s  12 peers, 40 seen");
        status.state = State::Paused;
        assert_eq!(status.describe(), " 25.0%  paused");
        assert!(status.pausable());
        status.state = State::Done;
        status.downloaded = status.size;
        assert_eq!(status.describe(), "100.0%  downloaded");
        assert!(!status.pausable());
    }

    #[test]
    fn status_json() {
        let status = Status {
            state: State::Paused,
            downloaded: 1,
            size: 2,
            download_speed: 0,
            peers: 0,
            seen: 3,
        };
        assert_eq!(
            serde_json::to_value(status).unwrap(),
            serde_json::json!({
                "state": "paused",
                "downloaded": 1,
                "size": 2,
                "download_speed": 0,
                "peers": 0,
                "seen": 3,
            })
        );
    }
}
