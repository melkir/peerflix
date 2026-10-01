//! Running the lib's async work, which needs tokio, from GPUI, whose executor
//! isn't tokio.

use std::sync::Arc;

use gpui_kit::{App, Global};
use librqbit::Session;
use peerflix_core::search;
use tokio::runtime::Handle;
use tokio_util::{
    sync::CancellationToken,
    task::{AbortOnDropHandle, TaskTracker},
};

/// The tokio runtime, the torrent session every stream shares and the client
/// every search shares, for as long as the app runs.
pub struct Runtime {
    pub tokio: Handle,
    pub session: Arc<Session>,
    pub client: search::Client,
    /// Cancelled as the app quits, which stops the session and every stream.
    pub shutdown: CancellationToken,
    /// The streams' tasks, which the app waits on as it quits, so they close
    /// their players and name their finished files.
    pub streams: TaskTracker,
}

impl Global for Runtime {}

/// Runs fut on tokio and returns a future of its output that GPUI can await.
/// Dropping the future aborts fut.
pub fn spawn<F>(cx: &App, fut: F) -> impl Future<Output = F::Output> + use<F>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    let task = AbortOnDropHandle::new(cx.global::<Runtime>().tokio.spawn(fut));
    async move {
        match task.await {
            Ok(out) => out,
            // The task is only aborted when this future is dropped, so the
            // error is a panic.
            Err(e) => std::panic::resume_unwind(e.into_panic()),
        }
    }
}
