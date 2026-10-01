//! Launching the player.

use std::path::PathBuf;

use anyhow::{Context, bail};

/// Joins paths into an mpv path list: colon separated, with a backslash
/// escaping a colon or backslash within a path.
fn mpv_path_list(paths: &[String]) -> String {
    let escaped: Vec<_> = paths
        .iter()
        .map(|p| p.replace('\\', "\\\\").replace(':', "\\:"))
        .collect();
    escaped.join(":")
}

/// An IINA player of its own, which closes when this is dropped.
///
/// iina-cli starts a new IINA process for each stream and, kept running, quits
/// with it. A SIGTERM to iina-cli closes that IINA too, which tokio's
/// kill_on_drop, a SIGKILL, doesn't.
pub struct Iina {
    cli: tokio::process::Child,
}

impl Iina {
    /// Opens the stream in a new IINA with the subtitle URLs.
    pub fn open(url: &str, subs: &[String]) -> anyhow::Result<Iina> {
        let bin = std::env::var_os("PATH")
            .and_then(|path| {
                std::env::split_paths(&path)
                    .map(|d| d.join("iina"))
                    .find(|p| p.is_file())
            })
            .or_else(|| {
                let app = PathBuf::from("/Applications/IINA.app/Contents/MacOS/iina-cli");
                app.is_file().then_some(app)
            })
            .context("IINA not found; install it with `brew install --cask iina`")?;
        let mut cmd = tokio::process::Command::new(bin);
        cmd.args(["--no-stdin", "--keep-running"]);
        if !subs.is_empty() {
            cmd.arg(format!("--mpv-sub-files={}", mpv_path_list(subs)));
        }
        let cli = cmd.arg(url).spawn().context("running IINA")?;
        Ok(Iina { cli })
    }

    /// Returns once the player quits.
    pub async fn wait(&mut self) -> anyhow::Result<()> {
        let status = self.cli.wait().await.context("running IINA")?;
        if !status.success() {
            bail!("IINA {status}");
        }
        Ok(())
    }
}

impl Drop for Iina {
    fn drop(&mut self) {
        // None once it has exited, so a reused pid is never signalled. A
        // negative pid would signal a whole process group, so it's checked.
        if let Some(pid) = self.cli.id().and_then(|p| libc::pid_t::try_from(p).ok()) {
            // SAFETY: kill only sends a signal.
            unsafe {
                libc::kill(pid, libc::SIGTERM);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mpv_path_lists() {
        let urls = ["http://127.0.0.1:8888/a.srt".to_owned(), r"b\c".to_owned()];
        assert_eq!(mpv_path_list(&urls), r"http\://127.0.0.1\:8888/a.srt:b\\c");
    }
}
