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

/// Opens the stream in IINA with the subtitle URLs and returns once the player
/// quits.
pub async fn launch_iina(url: String, subs: &[String]) -> anyhow::Result<()> {
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
    let status = cmd
        .arg(&url)
        .kill_on_drop(true)
        .status()
        .await
        .context("running IINA")?;
    if !status.success() {
        bail!("IINA {status}");
    }
    Ok(())
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
