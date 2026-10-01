//! peerflix in a window: search for anime, movies and series, and stream the
//! torrent into IINA.

mod runtime;
mod streams;
mod tables;
mod view;

use std::time::Duration;

use gpui_kit::{
    AppContext as _, Bounds, KeyBinding, Menu, MenuItem, TitlebarOptions, WindowBounds,
    WindowOptions, px, size,
};
use peerflix_core::{search, torrent, util};
use tokio_util::{sync::CancellationToken, task::TaskTracker};

use crate::{runtime::Runtime, view::Peerflix};

gpui_kit::actions!(
    peerflix,
    [
        /// Quits the app, stopping every stream.
        Quit,
        /// Selects the result above.
        SelectPrev,
        /// Selects the result below.
        SelectNext,
        /// Opens the selected result, or plays the selected episode.
        Confirm,
        /// Goes back to the results.
        Back,
        /// Searches the next category.
        NextCategory,
        /// Searches the previous category.
        PrevCategory,
    ]
);

fn main() {
    util::raise_open_file_limit();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("starting the tokio runtime");
    let shutdown = CancellationToken::new();
    let started = rt.block_on(async {
        let session =
            torrent::session(torrent::default_dir(), shutdown.child_token(), true).await?;
        anyhow::Ok((session, search::client()?))
    });
    let (session, client) = match started {
        Ok(started) => started,
        Err(e) => {
            eprintln!("error: {e:#}");
            std::process::exit(1);
        }
    };
    let runtime = Runtime {
        tokio: rt.handle().clone(),
        session,
        client,
        shutdown,
        streams: TaskTracker::new(),
    };

    gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .run(move |cx| {
            gpui_kit::init(cx);
            cx.set_global(runtime);
            cx.bind_keys([
                KeyBinding::new("cmd-q", Quit, None),
                // Typing goes to the search box, and the arrows and Enter to
                // the results.
                KeyBinding::new("up", SelectPrev, Some("Peerflix > Input")),
                KeyBinding::new("down", SelectNext, Some("Peerflix > Input")),
                KeyBinding::new("enter", Confirm, Some("Peerflix > DataTable")),
                KeyBinding::new("escape", Back, Some("Peerflix > Input")),
                // Tab and Shift-Tab switch category, as in the command line's
                // search.
                KeyBinding::new("tab", NextCategory, Some("Peerflix > Input")),
                KeyBinding::new("shift-tab", PrevCategory, Some("Peerflix > Input")),
                KeyBinding::new("escape", Back, Some("Peerflix > DataTable")),
            ]);
            cx.on_action(|_: &Quit, cx| cx.quit());
            cx.set_menus([Menu {
                name: "peerflix".into(),
                items: vec![MenuItem::action("Quit peerflix", Quit)],
                disabled: false,
            }]);
            // Stop the streams, which closes their players and names their
            // finished files, then the session, before quitting.
            cx.on_app_quit(|cx| {
                let rt = cx.global::<Runtime>();
                rt.shutdown.cancel();
                rt.streams.close();
                let (streams, session) = (rt.streams.clone(), rt.session.clone());
                runtime::spawn(cx, async move {
                    streams.wait().await;
                    session.stop().await;
                })
            })
            .detach();
            cx.on_window_closed(|cx, _| cx.quit()).detach();

            let bounds = Bounds::centered(None, size(px(1040.), px(680.)), cx);
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some("peerflix".into()),
                    ..Default::default()
                }),
                ..Default::default()
            };
            gpui_kit::open_window(options, cx, |window, cx| {
                cx.new(|cx| Peerflix::new(window, cx))
            })
            .expect("opening the window");
            cx.activate(true);
        });

    // Don't wait on librqbit's blocking disk tasks; the next run's data check
    // catches any piece they didn't finish writing.
    rt.shutdown_timeout(Duration::from_secs(1));
}
