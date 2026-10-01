//! Streaming a torrent from start to end: listing its files, downloading the
//! one picked with its subtitles while serving them, and naming the files
//! that finish. The command line and the app both stream through it.

use std::{path::Path, sync::Arc, time::Duration};

use anyhow::{Context, bail};
use futures_util::future::BoxFuture;
use librqbit::{AddTorrent, ListOnlyResponse, ManagedTorrent, Session, api::TorrentIdOrHash};
use serde::Serialize;
use tokio_util::sync::CancellationToken;

use crate::{
    http::{Download, Server},
    torrent::{
        Status, Stream, add_torrent, complete_files, fetch_metadata,
        files::{TorrentFile, episodes, subtitles, torrent_files},
        serve_files,
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
        let subs = subtitles(files, id, episodes.len() <= 1);
        let downloading: Vec<usize> = [id].into_iter().chain(subs.iter().copied()).collect();
        let storage = PartStorage::new(meta.output_folder.clone());
        let torrent = add_torrent(session, meta, &storage, &downloading).await?;
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
            files: files.clone(),
            downloading,
        })
    }
}

/// A file being downloaded and served with its subtitles.
pub struct Playing {
    pub stream: Stream,
    control: Control,
    storage: PartStorage,
    files: Vec<TorrentFile>,
    /// The files still downloading, the one streamed among them.
    downloading: Vec<usize>,
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
    /// renaming the files that finish, until cancelled or player returns,
    /// and returns the player's result. The torrent is then removed from the
    /// session, keeping its files, so it stops downloading and can play
    /// again.
    pub async fn watch(
        mut self,
        cancel: &CancellationToken,
        player: impl Future<Output = anyhow::Result<()>>,
        mut on_status: impl FnMut(&Status),
    ) -> anyhow::Result<()> {
        tokio::pin!(player);
        let mut ticker = tokio::time::interval_at(
            tokio::time::Instant::now() + Duration::from_secs(1),
            Duration::from_secs(1),
        );
        let torrent = self.control.torrent.clone();
        let mut init = torrent.wait_until_initialized();
        let mut initialized = false;
        let result = loop {
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
                    self.complete_files();
                    on_status(&self.control.status());
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

    /// Drops their .part suffix from the files that have finished.
    fn complete_files(&mut self) {
        let stats = self.control.torrent.stats();
        complete_files(&self.storage, &stats, &self.files, &mut self.downloading);
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
    /// The status, with the line describing it as text.
    fn status_json(&self) -> String {
        #[derive(Serialize)]
        struct Json {
            #[serde(flatten)]
            status: Status,
            text: String,
        }
        let status = self.status();
        let text = status.describe();
        serde_json::to_string(&Json { status, text }).expect("the status serializes")
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
    use crate::torrent::{Served, State};

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
