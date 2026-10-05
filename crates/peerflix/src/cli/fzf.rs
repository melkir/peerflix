//! Picking from lists in fzf: the live search and the episode picker.

use std::{
    io::{IsTerminal, Write},
    process::Stdio,
    sync::Arc,
    time::Duration,
};

use anyhow::Context;
use peerflix_core::{
    providers::{Provider, Query, Torrent},
    search::{self, Category},
    torrent::files::{TorrentFile, pick_file},
    util::{human_bytes, terminate},
};
use tokio::io::AsyncWriteExt;
use tokio_util::sync::CancellationToken;

/// Options both lists share. Lines are tab separated: the value to return,
/// which is hidden, then columns shown with ANSI colors, of which the second
/// shown (the title or path) is what typing matches, exactly, ignoring case,
/// in the order the lines came. Long lines wrap.
const LIST: [&str; 15] = [
    "--ansi",
    "--exact",
    "-i",
    "--no-sort",
    "--tabstop",
    "1",
    "--wrap",
    "--delimiter",
    "\t",
    "--with-nth",
    "2..",
    // Counted after --with-nth hides the value.
    "--nth",
    "2",
    "--accept-nth",
    "1",
];

/// The user quit the search without picking a torrent.
#[derive(Debug)]
pub struct NoSelection;

impl std::fmt::Display for NoSelection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("nothing selected")
    }
}

impl std::error::Error for NoSelection {}

/// Runs fzf over live searches, starting in category, with nyaa optionally
/// restricted to one uploader or to trusted uploads, and returns the chosen
/// torrent URL or magnet, or NoSelection if the user quits. Tab and
/// Shift-Tab switch category, keeping the query.
pub async fn search_interactive(
    initial: &str,
    category: Category,
    user: &str,
    trusted: bool,
) -> anyhow::Result<String> {
    let exe = std::env::current_exe()?;
    // The prompt holds the category, as in "movies> ".
    let mut search =
        shell_quote(&exe.to_string_lossy()) + r#" --print --category "${FZF_PROMPT%> }""#;
    if !user.is_empty() {
        search += " --user ";
        search += &shell_quote(user);
    }
    if trusted {
        search += " --trusted";
    }
    search += " -- {q}";

    // fzf filters the current list on every keystroke, matching title terms
    // in the order the sites returned them, while a reload fetches the
    // results for the new query. fzf kills a running reload when the next
    // one starts, so the sleep debounces typing.
    //
    // Each search writes a status line to PEERFLIX_STATUS, shown as the
    // header once its results have loaded.
    //
    // Tab changes the prompt and reloads in one transform, since a reload
    // chained after change-prompt still sees the old prompt. The search
    // command comes from PEERFLIX_SEARCH so that fzf fills in its {q} at
    // reload time, quoted, rather than inside the transform's own command.
    let cycle = |order: [&str; 3]| {
        format!(
            r#"transform:case $FZF_PROMPT in {0}*) n={1};; {1}*) n={2};; *) n={0};; esac; echo "change-prompt($n> )+reload:$PEERFLIX_SEARCH""#,
            order[0], order[1], order[2]
        )
    };
    let status = std::env::temp_dir().join(format!("peerflix-{}.status", std::process::id()));
    let out = tokio::process::Command::new("fzf")
        .env("PEERFLIX_SEARCH", &search)
        .env("PEERFLIX_STATUS", &status)
        .args(LIST)
        .args(["--query", initial])
        .args(["--prompt", &format!("{}> ", category.name())])
        .args(["--with-shell", "sh -c"])
        .args(["--bind", "enter:accept-non-empty"])
        .args([
            "--bind",
            &format!("tab:{}", cycle(["anime", "movies", "series"])),
        ])
        .args([
            "--bind",
            &format!("shift-tab:{}", cycle(["series", "movies", "anime"])),
        ])
        .args(["--bind", &format!("start:reload:{search}")])
        .args(["--bind", &format!("change:reload:sleep 0.25; {search}")])
        .args([
            "--bind",
            r#"load:transform-header:cat "$PEERFLIX_STATUS" 2>/dev/null"#,
        ])
        .stdin(Stdio::inherit())
        .stderr(Stdio::inherit())
        .output()
        .await
        .context(FZF);
    let _ = std::fs::remove_file(&status);
    fzf_choice(&out?)
}

/// Searches providers, category's sites, for query in parallel and writes
/// their results as the live search's input as each site answers, one line
/// per result.
///
/// Returns a status line for the search's header: which sites didn't answer,
/// or where else to look when nothing was found, or nothing when all went
/// well.
pub async fn print_results(
    w: &mut impl Write,
    client: &search::Client,
    category: Category,
    providers: &[Arc<dyn Provider>],
    query: &str,
) -> String {
    let mut printed = 0;
    let failed = search::search(client, providers, &Query::new(query), |site, items| {
        // The site column is only worth its space when there are several.
        let site = (providers.len() > 1).then_some(site);
        for it in items {
            // A closed pipe just means fzf moved on to the next query.
            let _ = writeln!(w, "{}", line(&it, site));
            printed += 1;
        }
        let _ = w.flush();
    })
    .await;
    search::summary(category, query, printed, &failed, providers.len())
}

/// The live search's line for it: the torrent URL, a tab, the date, size,
/// seeders (colored by health) and site if given, a tab, and the title.
fn line(it: &Torrent, site: Option<&str>) -> String {
    let site = site.map_or_else(String::new, |s| format!("  {s:<4}"));
    format!(
        "{}\t\x1b[90m{}  {:>10}\x1b[0m  {}{:>5}\x1b[90m{site}\x1b[0m \t{}",
        it.url,
        it.date,
        human_bytes(it.size),
        health(it),
        it.seeders,
        one_line(&it.title)
    )
}

/// Text from a site or a torrent, with its tabs, newlines and other control
/// characters made spaces, so it keeps to its column of one line.
fn one_line(s: &str) -> String {
    s.replace(char::is_control, " ")
}

/// Rates a live torrent by its seeders relative to its leechers, as the
/// color to print its seeder count in.
fn health(it: &Torrent) -> &'static str {
    use std::cmp::Ordering::*;
    match it.seeders.cmp(&it.leechers) {
        Greater => "\x1b[32m",
        Equal => "\x1b[33m",
        Less => "\x1b[38;5;208m",
    }
}

/// Reports whether the file to stream is picked in fzf: when no index is
/// given, eps, the torrent's episodes, are several, and stdin is a terminal.
pub fn picks_episode(eps: &[usize], index: Option<usize>) -> bool {
    index.is_none() && eps.len() > 1 && std::io::stdin().is_terminal()
}

/// Returns the file to stream: the one at index if given, else one the user
/// picks in fzf among eps, the torrent's episodes, when picks_episode, with
/// the cursor on played, else the one pick_file chooses. None means peerflix
/// was cancelled meanwhile.
pub async fn select_file(
    cancel: &CancellationToken,
    files: &[TorrentFile],
    eps: &[usize],
    index: Option<usize>,
    played: Option<usize>,
) -> anyhow::Result<Option<usize>> {
    if !picks_episode(eps, index) {
        return pick_file(files, index).map(Some);
    }
    let lines: String = eps
        .iter()
        .map(|&i| {
            let f = &files[i];
            format!(
                "{i}\t\x1b[90m{:>9}\x1b[0m  \t{}\n",
                human_bytes(f.len),
                one_line(&f.path)
            )
        })
        .collect();
    let pos = played.and_then(|id| eps.iter().position(|&e| e == id));
    let Some(choice) = choose(cancel, "episode> ", lines, pos.unwrap_or(0)).await? else {
        return Ok(None);
    };
    Ok(Some(choice.parse().context("reading fzf's choice")?))
}

/// Runs fzf over lines, each the value to return, a tab, a detail column, a
/// tab and the text to match, with the cursor starting on the line at pos,
/// and returns the chosen value, NoSelection if the user quits, or None if
/// cancel is cancelled first.
pub async fn choose(
    cancel: &CancellationToken,
    prompt: &str,
    lines: String,
    pos: usize,
) -> anyhow::Result<Option<String>> {
    let mut child = tokio::process::Command::new("fzf")
        .args(LIST)
        .args(["--prompt", prompt])
        // fzf counts from 1.
        .args(["--bind", &format!("load:pos({})", pos + 1)])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .context(FZF)?;
    let mut stdin = child.stdin.take().context(FZF)?;
    // fzf may quit before reading everything.
    let _ = stdin.write_all(lines.as_bytes()).await;
    drop(stdin);
    let pid = child.id();
    let out = child.wait_with_output();
    tokio::pin!(out);
    tokio::select! {
        out = &mut out => fzf_choice(&out.context(FZF)?).map(Some),
        () = cancel.cancelled() => {
            // kill_on_drop is left for an fzf that doesn't exit.
            terminate(pid);
            let _ = tokio::time::timeout(Duration::from_secs(1), out).await;
            Ok(None)
        }
    }
}

const FZF: &str = "running fzf (0.60 or later is required)";

/// Returns what fzf printed for the accepted line, or NoSelection if the user
/// quit or nothing matched.
fn fzf_choice(out: &std::process::Output) -> anyhow::Result<String> {
    match out.status.code() {
        Some(0) => {}
        Some(1 | 130) => return Err(NoSelection.into()), // no match, or Esc/Ctrl-C
        _ => anyhow::bail!("{FZF}: {}", out.status),
    }
    let choice = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    if choice.is_empty() {
        return Err(NoSelection.into());
    }
    Ok(choice)
}

/// Quotes s for sh.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn lines() {
        let it = Torrent {
            url: "https://nyaa.si/download/1.torrent".into(),
            title: "[Group] Big Buck Bunny - 01 [1080p].mkv".into(),
            date: "2026-09-26".into(),
            size: 1_288_490_188,
            seeders: 42,
            leechers: 3,
            ..Torrent::default()
        };
        let l = line(&it, None);
        let fields: Vec<_> = l.split('\t').collect();
        assert_eq!(fields.len(), 3, "{l:?}");
        assert_eq!(fields[0], "https://nyaa.si/download/1.torrent");
        assert!(
            fields[1].contains("2026-09-26")
                && fields[1].contains("1.2 GiB")
                // 42 seeders and 3 leechers: green.
                && fields[1].contains("\x1b[32m   42"),
            "{:?}",
            fields[1]
        );
        assert_eq!(fields[2], "[Group] Big Buck Bunny - 01 [1080p].mkv");

        // A title's tabs and newlines would make columns and lines of their own.
        let odd = Torrent {
            title: "Big\tBuck\nBunny".into(),
            ..Torrent::default()
        };
        let l = line(&odd, None);
        assert_eq!(l.split('\t').count(), 3, "{l:?}");
        assert!(l.ends_with("\tBig Buck Bunny"), "{l:?}");

        let l = line(&it, Some("yts"));
        assert!(l.contains("   42\x1b[90m  yts \x1b[0m \t"), "{l:?}");
    }

    #[test]
    fn health_colors() {
        for (seeders, leechers, color) in [
            (10, 2, "\x1b[32m"),
            (3, 3, "\x1b[33m"),
            (1, 9, "\x1b[38;5;208m"),
        ] {
            let it = Torrent {
                seeders,
                leechers,
                ..Torrent::default()
            };
            assert!(
                health(&it) == color,
                "{seeders} seeders, {leechers} leechers"
            );
        }
    }
}
