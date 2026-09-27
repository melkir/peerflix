use std::{
    io::Write,
    process::{Command, Stdio},
};

use anyhow::Context;

use crate::nyaa::{self, Item};

/// The user quit the search without picking a torrent.
#[derive(Debug)]
pub struct NoSelection;

impl std::fmt::Display for NoSelection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("nothing selected")
    }
}

impl std::error::Error for NoSelection {}

/// Writes nyaa's results for query as fzf input, one tab separated line per
/// result: the torrent URL, the date, size and health, and the title. A
/// failed search, such as nyaa being unavailable or rate limiting, prints
/// nothing.
pub async fn print_results(w: &mut impl Write, base: &str, query: &str, user: &str, trusted: bool) {
    let items = nyaa::search(base, query, user, trusted)
        .await
        .unwrap_or_default();
    for it in items {
        // A closed pipe just means fzf moved on to the next query.
        let _ = writeln!(
            w,
            "{}\t\x1b[90m{}  {:>10}\x1b[0m  {} \t{}",
            it.torrent,
            it.date(),
            it.size,
            health(&it),
            it.title
        );
    }
}

/// Rates a torrent by its seeders relative to its leechers.
pub fn health(it: &Item) -> &'static str {
    use std::cmp::Ordering::*;
    match (it.seeders, it.seeders.cmp(&it.leechers)) {
        (0, _) => "\x1b[31m●\x1b[0m",
        (_, Greater) => "\x1b[32m●\x1b[0m",
        (_, Equal) => "\x1b[33m●\x1b[0m",
        (_, Less) => "\x1b[38;5;208m●\x1b[0m",
    }
}

/// Runs fzf over live nyaa searches, optionally restricted to one uploader or
/// to trusted uploads, and returns the chosen torrent URL, or NoSelection if
/// the user quits.
pub fn search_interactive(initial: &str, user: &str, trusted: bool) -> anyhow::Result<String> {
    let exe = std::env::current_exe()?;
    let mut search = shell_quote(&exe.to_string_lossy()) + " --print";
    if !user.is_empty() {
        search += &format!(" --user {}", shell_quote(user));
    }
    if trusted {
        search += " --trusted";
    }
    search += " -- {q}";

    // fzf filters the current list on every keystroke, matching title terms
    // in nyaa's newest-first order, while a reload fetches nyaa's results for
    // the new query. fzf kills a running reload when the next one starts, so
    // the sleep debounces typing.
    let out = Command::new("fzf")
        .args(["--ansi", "--exact", "-i", "--no-sort", "--tabstop", "1"])
        .args(["--query", initial])
        .args(["--prompt", "nyaa> "])
        .args(["--with-shell", "sh -c"])
        .args([
            "--delimiter",
            "\t",
            "--with-nth",
            "2..",
            "--nth",
            "2",
            "--accept-nth",
            "1",
        ])
        .args(["--bind", "enter:accept-non-empty"])
        .args(["--bind", &format!("start:reload:{search}")])
        .args(["--bind", &format!("change:reload:sleep 0.25; {search}")])
        .stdin(Stdio::inherit())
        .stderr(Stdio::inherit())
        .output()
        .context("running fzf (0.60 or later is required)")?;
    match out.status.code() {
        Some(0) => {}
        Some(1 | 130) => return Err(NoSelection.into()), // no match, or Esc/Ctrl-C
        _ => anyhow::bail!("running fzf (0.60 or later is required): {}", out.status),
    }
    let url = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    if url.is_empty() {
        return Err(NoSelection.into());
    }
    Ok(url)
}

/// Quotes s for sh.
pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{nyaa::tests::SAMPLE_FEED, testutil::FakeServer};

    #[tokio::test]
    async fn prints_results() {
        let srv = FakeServer::start(200, SAMPLE_FEED).await;
        let mut buf = Vec::new();
        print_results(&mut buf, &srv.url, "bunny", "", false).await;
        let out = String::from_utf8(buf).unwrap();
        let lines: Vec<_> = out.lines().collect();
        assert_eq!(lines.len(), 2, "{out}");
        let fields: Vec<_> = lines[0].split('\t').collect();
        assert_eq!(fields.len(), 3, "{:?}", lines[0]);
        assert_eq!(fields[0], "https://nyaa.si/download/1.torrent");
        assert!(
            fields[1].contains("2026-09-26") && fields[1].contains("1.2 GiB"),
            "{:?}",
            fields[1]
        );
        assert_eq!(fields[2], "[Group] Big Buck Bunny - 01 [1080p].mkv");
    }

    #[tokio::test]
    async fn failed_search_prints_nothing() {
        let srv = FakeServer::start(503, "").await;
        let mut buf = Vec::new();
        print_results(&mut buf, &srv.url, "bunny", "", false).await;
        assert!(buf.is_empty());
    }

    #[test]
    fn health_colors() {
        for (seeders, leechers, color) in [
            (0, 5, "\x1b[31m"),
            (0, 0, "\x1b[31m"),
            (10, 2, "\x1b[32m"),
            (3, 3, "\x1b[33m"),
            (1, 9, "\x1b[38;5;208m"),
        ] {
            let it = Item {
                seeders,
                leechers,
                ..Item::default()
            };
            assert!(
                health(&it).starts_with(color),
                "{seeders} seeders, {leechers} leechers"
            );
        }
    }

    #[test]
    fn quotes_for_sh() {
        for (s, want) in [
            ("", "''"),
            ("plain", "'plain'"),
            ("/path with space", "'/path with space'"),
            ("it's", r"'it'\''s'"),
        ] {
            assert_eq!(shell_quote(s), want);
        }
    }
}
