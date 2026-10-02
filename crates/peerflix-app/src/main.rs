//! peerflix in a window: search for anime, movies and series, and stream the
//! torrent into IINA.

mod runtime;
mod streams;
mod tables;
mod theme;
mod view;

use std::time::Duration;

use anyhow::Context as _;
use gpui_kit::{
    AppContext as _, AssetSource, Bounds, KeyBinding, Menu, MenuItem, SharedString,
    TitlebarOptions, WindowBounds, WindowOptions, component::TitleBar, px, size,
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

// The icons the window uses beyond gpui-component's own.
gpui_kit::assets::icon_assets!(ExtraIcons, [SearchX, WifiOff]);

/// gpui-component's icons, and ExtraIcons.
struct AppAssets;

impl AssetSource for AppAssets {
    fn load(&self, path: &str) -> gpui_kit::Result<Option<std::borrow::Cow<'static, [u8]>>> {
        match ExtraIcons.load(path)? {
            Some(bytes) => Ok(Some(bytes)),
            None => gpui_kit::assets::Assets.load(path),
        }
    }

    fn list(&self, path: &str) -> gpui_kit::Result<Vec<SharedString>> {
        let mut paths = gpui_kit::assets::Assets.list(path)?;
        paths.extend(ExtraIcons.list(path)?);
        paths.sort();
        paths.dedup();
        Ok(paths)
    }
}

fn main() {
    util::raise_open_file_limit();
    let (rt, runtime) = match start() {
        Ok(started) => started,
        Err(e) => {
            eprintln!("error: {e:#}");
            std::process::exit(1);
        }
    };

    gpui_kit::application()
        .with_assets(AppAssets)
        .run(move |cx| {
            gpui_kit::init(cx);
            theme::init(cx);
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
                name: "Peerflix".into(),
                items: vec![MenuItem::action("Quit Peerflix", Quit)],
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
            // The window draws its own title bar, a toolbar as Transmission's,
            // with the traffic lights centered in it.
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    // Hidden, but named in the Dock's menu and Mission Control.
                    title: Some("Peerflix".into()),
                    traffic_light_position: Some(view::TRAFFIC_LIGHTS),
                    ..TitleBar::title_bar_options()
                }),
                ..TitleBar::window_options()
            };
            let opened = gpui_kit::open_window(options, cx, |window, cx| {
                cx.new(|cx| Peerflix::new(window, cx))
            });
            if let Err(e) = opened {
                eprintln!("error: opening the window: {e:#}");
                return cx.quit();
            }
            cx.activate(true);
        });

    // Don't wait on librqbit's blocking disk tasks; the next run's data check
    // catches any piece they didn't finish writing.
    rt.shutdown_timeout(Duration::from_secs(1));
}

/// Starts tokio, and on it the torrent session and the search client.
fn start() -> anyhow::Result<(tokio::runtime::Runtime, Runtime)> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("starting the tokio runtime")?;
    let shutdown = CancellationToken::new();
    let (session, client) = rt.block_on(async {
        let session =
            torrent::session(torrent::default_dir(), shutdown.child_token(), true).await?;
        anyhow::Ok((session, search::client()?))
    })?;
    let runtime = Runtime {
        tokio: rt.handle().clone(),
        session,
        client,
        shutdown,
        streams: TaskTracker::new(),
    };
    Ok((rt, runtime))
}
