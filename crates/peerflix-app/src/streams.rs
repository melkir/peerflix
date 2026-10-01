//! The streams started from the window, each a row with its download's
//! progress and buttons to pause and stop it, and streaming each into an IINA
//! of its own, on tokio.

use std::{sync::Arc, time::Duration};

use gpui_kit::component::{
    ActiveTheme as _, IconName, Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    progress::Progress,
    v_flex,
};
use gpui_kit::{
    Context, InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString,
    Styled as _, Task, Window, div, prelude::FluentBuilder as _,
};
use librqbit::Session;
use peerflix_core::{
    http::{Server, bind_listener},
    play::{Control, Listing},
    player::Iina,
    torrent::{State, Status},
};
use tokio::{
    sync::mpsc::{self, UnboundedSender},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

use crate::runtime::{self, Runtime};

/// How long a stream goes without a player connected before it ends, for a
/// player that lingers without its window: IINA keeps its connection while
/// the video is open, paused or not.
const IDLE: Duration = Duration::from_secs(30);

#[derive(Default)]
pub struct Streams {
    rows: Vec<Row>,
    next_id: usize,
}

struct Row {
    id: usize,
    /// The torrent's, which plays once at a time.
    info_hash: String,
    /// The torrent's title until the file's name is known.
    name: SharedString,
    state: RowState,
    stop: CancellationToken,
    /// The task streaming, until a stream of the same torrent waits on it.
    task: Option<JoinHandle<()>>,
    _events: Task<()>,
}

enum RowState {
    /// Adding the torrent, until its file is served.
    Starting,
    /// Served and open in IINA, with the download's latest status once
    /// there's one.
    Playing {
        control: Control,
        status: Option<Status>,
    },
    /// Told to stop, until it has ended.
    Stopping,
    /// Ended with this error, until dismissed.
    Failed(SharedString),
}

/// What a stream tells the window.
enum Event {
    /// The file is served under name and open in IINA, and its download is
    /// followed and paused with control.
    Playing {
        name: String,
        control: Control,
    },
    Status(Status),
    /// The stream ended, on its own or stopped, or why it failed.
    Ended(Result<(), String>),
}

impl Streams {
    /// Streams file id of listing, the files of the torrent titled title,
    /// stopping the stream of the same torrent first if there's one.
    pub fn play(
        &mut self,
        title: SharedString,
        listing: Arc<Listing>,
        id: usize,
        cx: &mut Context<Self>,
    ) {
        let info_hash = listing.info_hash();
        let previous = self
            .rows
            .iter_mut()
            .find(|r| r.info_hash == info_hash && r.task.is_some())
            .and_then(|row| {
                if !matches!(row.state, RowState::Failed(_)) {
                    row.state = RowState::Stopping;
                }
                row.stop.cancel();
                row.task.take()
            });
        let rt = cx.global::<Runtime>();
        let stop = rt.shutdown.child_token();
        let (events, mut received) = mpsc::unbounded_channel();
        let (session, run_stop) = (rt.session.clone(), stop.clone());
        let task = rt.streams.spawn_on(
            async move {
                if let Some(previous) = previous {
                    let _ = previous.await;
                }
                let result = stream(&session, &listing, id, &run_stop, &events).await;
                let _ = events.send(Event::Ended(result.map_err(|e| format!("{e:#}"))));
            },
            &rt.tokio,
        );
        let id = self.next_id;
        self.next_id += 1;
        let events = cx.spawn(async move |this, cx| {
            while let Some(event) = received.recv().await {
                if this
                    .update(cx, |this, cx| this.on_event(id, event, cx))
                    .is_err()
                {
                    return;
                }
            }
        });
        self.rows.push(Row {
            id,
            info_hash,
            name: title,
            state: RowState::Starting,
            stop,
            task: Some(task),
            _events: events,
        });
        cx.notify();
    }

    fn row(&mut self, id: usize) -> Option<&mut Row> {
        self.rows.iter_mut().find(|r| r.id == id)
    }

    fn on_event(&mut self, id: usize, event: Event, cx: &mut Context<Self>) {
        let Some(row) = self.row(id) else {
            return;
        };
        match event {
            // A stream told to stop meanwhile stays stopping.
            Event::Playing { name, control } => {
                row.name = name.into();
                if matches!(row.state, RowState::Starting) {
                    row.state = RowState::Playing {
                        control,
                        status: None,
                    };
                }
            }
            Event::Status(new) => {
                if let RowState::Playing { status, .. } = &mut row.state {
                    *status = Some(new);
                }
            }
            Event::Ended(Ok(())) => self.rows.retain(|r| r.id != id),
            Event::Ended(Err(e)) => row.state = RowState::Failed(e.into()),
        }
        cx.notify();
    }

    /// Pauses stream id's download, or resumes it.
    fn toggle_pause(&mut self, id: usize, cx: &mut Context<Self>) {
        let Some(RowState::Playing {
            control,
            status: Some(status),
        }) = self.row(id).map(|r| &r.state)
        else {
            return;
        };
        let pause = status.state != State::Paused;
        let control = control.clone();
        let done = runtime::spawn(cx, async move { control.set_paused(pause).await });
        cx.spawn(async move |this, cx| {
            // A failure shows as the state not changing.
            if done.await.is_ok() {
                let _ = this.update(cx, |this, cx| {
                    // Shown now rather than at the next status.
                    if let Some(RowState::Playing {
                        status: Some(status),
                        ..
                    }) = this.row(id).map(|r| &mut r.state)
                    {
                        status.state = if pause {
                            State::Paused
                        } else {
                            State::Downloading
                        };
                        cx.notify();
                    }
                });
            }
        })
        .detach();
    }

    /// Stops stream id, or forgets it once it has failed.
    fn stop(&mut self, id: usize, cx: &mut Context<Self>) {
        let Some(row) = self.row(id) else {
            return;
        };
        if let RowState::Failed(_) = row.state {
            self.rows.retain(|r| r.id != id);
        } else {
            // It's removed once it has ended.
            row.state = RowState::Stopping;
            row.stop.cancel();
        }
        cx.notify();
    }

    fn render_row(&self, row: &Row, cx: &mut Context<Self>) -> impl IntoElement {
        let id = row.id;
        let theme = cx.theme();
        let (line, status): (SharedString, _) = match &row.state {
            RowState::Starting | RowState::Playing { status: None, .. } => {
                ("Starting…".into(), None)
            }
            RowState::Playing {
                status: Some(s), ..
            } => (s.describe().into(), Some(s)),
            RowState::Stopping => ("Stopping…".into(), None),
            RowState::Failed(e) => (e.clone(), None),
        };
        let failed = matches!(row.state, RowState::Failed(_));
        let pausable = status.is_some_and(|s| s.pausable());
        let paused = status.is_some_and(|s| s.state == State::Paused);
        h_flex()
            .id(("stream", id))
            .gap_3()
            .px_3()
            .py_2()
            .border_t_1()
            .border_color(theme.border)
            .child(div().flex_1().min_w_0().truncate().child(row.name.clone()))
            .child(
                div().w_40().child(
                    Progress::new(("progress", id))
                        .small()
                        .loading(!failed && status.is_none_or(|s| s.state == State::Checking))
                        .value(status.map_or(0., |s| s.percent() as f32)),
                ),
            )
            .child(
                div()
                    .w_72()
                    .truncate()
                    .text_sm()
                    .text_color(if failed {
                        theme.danger
                    } else {
                        theme.muted_foreground
                    })
                    .child(line),
            )
            .child(
                h_flex()
                    .w_16()
                    .justify_end()
                    .gap_1()
                    .when(pausable, |el| {
                        el.child(
                            Button::new(("pause", id))
                                .ghost()
                                .xsmall()
                                .icon(if paused {
                                    IconName::Play
                                } else {
                                    IconName::Pause
                                })
                                .tooltip(if paused {
                                    "Resume download"
                                } else {
                                    "Pause download"
                                })
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.toggle_pause(id, cx);
                                })),
                        )
                    })
                    .when(!matches!(row.state, RowState::Stopping), |el| {
                        el.child(
                            Button::new(("stop", id))
                                .ghost()
                                .xsmall()
                                .icon(IconName::Close)
                                .tooltip(if failed { "Dismiss" } else { "Stop" })
                                .on_click(cx.listener(move |this, _, _, cx| this.stop(id, cx))),
                        )
                    }),
            )
    }
}

impl Render for Streams {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let rows: Vec<_> = self
            .rows
            .iter()
            .map(|row| self.render_row(row, cx).into_any_element())
            .collect();
        v_flex().children(rows)
    }
}

/// Streams file id of listing into an IINA of its own until stop is
/// cancelled, the player quits, or no player has been connected for IDLE,
/// sending how it's going to events. The player closes as the stream ends.
async fn stream(
    session: &Arc<Session>,
    listing: &Listing,
    id: usize,
    stop: &CancellationToken,
    events: &UnboundedSender<Event>,
) -> anyhow::Result<()> {
    // A port of its own, so streams can play side by side.
    let (server, _) = Server::start(bind_listener(Some(0)).await?, stop.clone())?;
    let Some(playing) = stop
        .run_until_cancelled(listing.play(session, &server, id))
        .await
    else {
        return Ok(());
    };
    let playing = playing?;
    let stream = &playing.stream;
    let _ = events.send(Event::Playing {
        name: stream.video.name.clone(),
        control: playing.control(),
    });
    let url = stream.video.url.clone();
    let subs: Vec<_> = stream.subtitles.iter().map(|s| s.url.clone()).collect();
    let connections = server.connections();
    // Opened in here, so it's closed when watching stops, and failing to open
    // it still removes the torrent.
    let player = async {
        let mut iina = Iina::open(&url, &subs)?;
        tokio::select! {
            quit = iina.wait() => quit,
            () = connections.idle(IDLE) => Ok(()),
        }
    };
    playing
        .watch(stop, player, |status| {
            let _ = events.send(Event::Status(*status));
        })
        .await
}
