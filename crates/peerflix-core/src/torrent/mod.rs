//! The librqbit session torrents stream from, what a stream serves, and how
//! its download is going.

pub mod files;
pub mod storage;

use std::{path::PathBuf, sync::Arc};

use anyhow::Context;
use librqbit::{
    DhtSessionConfig, ListenerOptions, Session, SessionOptions, TorrentStats, TorrentStatsState,
};
use serde::Serialize;
use tokio_util::sync::CancellationToken;

use crate::util::human_bytes;

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
    /// The download hit an error it doesn't recover from, such as a full
    /// disk.
    Failed,
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
            TorrentStatsState::Error => State::Failed,
            _ if downloaded >= size => State::Done,
            TorrentStatsState::Paused => State::Paused,
            TorrentStatsState::Live => State::Downloading,
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
            State::Failed => "Download failed".into(),
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
    fn states_from_stats() {
        let status = |state, downloaded| {
            let stats = TorrentStats {
                state,
                file_progress: vec![downloaded],
                error: None,
                progress_bytes: 0,
                uploaded_bytes: 0,
                total_bytes: 0,
                finished: false,
                live: None,
            };
            Status::new(&stats, 0, 10).state
        };
        let initializing = TorrentStatsState::Initializing { paused: false };
        for (state, downloaded, want) in [
            (initializing, 10, State::Checking),
            (TorrentStatsState::Live, 5, State::Downloading),
            (TorrentStatsState::Live, 10, State::Done),
            (TorrentStatsState::Paused, 5, State::Paused),
            (TorrentStatsState::Paused, 10, State::Done),
            // Even with the file all there, as the stream ends.
            (TorrentStatsState::Error, 10, State::Failed),
        ] {
            assert_eq!(status(state, downloaded), want, "{downloaded}");
        }
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
