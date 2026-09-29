//! Picking from lists in fzf: the live search and the episode picker.

use std::{
    io::Write,
    process::{Command, Stdio},
    sync::Arc,
};

use anyhow::Context;
use peerflix::{
    provider::{Provider, Query, Torrent},
    search::{self, Category, Failed},
};
use tokio::io::AsyncWriteExt;

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
pub fn search_interactive(
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
        search += &format!(" --user {}", shell_quote(user));
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
    let out = Command::new("fzf")
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
        .context(FZF);
    let _ = std::fs::remove_file(&status);
    fzf_choice(out?)
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
    category: Category,
    providers: &[Arc<dyn Provider>],
    query: &str,
) -> String {
    let mut printed = 0;
    let failed = search::search(providers, &Query::new(query), |site, items| {
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
    status(category, query, printed, &failed, providers.len())
}

/// The live search's line for it: the torrent URL, a tab, the date, size,
/// seeders (colored by health) and site if given, a tab, and the title.
fn line(it: &Torrent, site: Option<&str>) -> String {
    let site = site.map_or_else(String::new, |s| format!("  {s:<4}"));
    format!(
        "{}\t\x1b[90m{}  {:>10}\x1b[0m  {}{:>5}\x1b[90m{site}\x1b[0m \t{}",
        it.url,
        it.date,
        it.size,
        health(it),
        it.seeders,
        it.title
    )
}

/// The header line for a search that printed printed results, with failed
/// holding the sites, out of sites, whose search failed.
fn status(
    category: Category,
    query: &str,
    printed: usize,
    failed: &Failed,
    sites: usize,
) -> String {
    let unanswered = failed.unanswered.join(" and ");
    if failed.unanswered.len() == sites {
        return format!("{unanswered} didn't answer. Check your connection and try again.");
    }
    let mut notes: Vec<String> = failed.errors.iter().map(|e| format!("{e}.")).collect();
    if !unanswered.is_empty() {
        notes.insert(0, format!("{unanswered} didn't answer."));
    }
    let missing = notes.join(" ");
    // No results says nothing when no site could search.
    if printed > 0 || failed.count() == sites {
        return missing;
    }
    let missing = if missing.is_empty() {
        missing
    } else {
        missing + " "
    };
    let [a, b] = category.others().map(Category::name);
    let query = query.trim();
    let none = if query.is_empty() {
        format!("No {} to show.", category.name())
    } else {
        format!("No {} results for \"{query}\".", category.name())
    };
    format!("{missing}{none} Tab searches {a} and {b}.")
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

/// Runs fzf over lines, each the value to return, a tab, a detail column, a
/// tab and the text to match, and returns the chosen value, or NoSelection if
/// the user quits.
pub async fn choose(prompt: &str, lines: String) -> anyhow::Result<String> {
    let mut child = tokio::process::Command::new("fzf")
        .args(LIST)
        .args(["--prompt", prompt])
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
    fzf_choice(child.wait_with_output().await.context(FZF)?)
}

const FZF: &str = "running fzf (0.60 or later is required)";

/// Returns what fzf printed for the accepted line, or NoSelection if the user
/// quit or nothing matched.
fn fzf_choice(out: std::process::Output) -> anyhow::Result<String> {
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
            size: "1.2 GiB".into(),
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

        let l = line(&it, Some("yts"));
        assert!(l.contains("   42\x1b[90m  yts \x1b[0m \t"), "{l:?}");
    }

    #[test]
    fn statuses() {
        use Category::*;
        let failed = |unanswered: &[&'static str], errors: &[&str]| Failed {
            unanswered: unanswered.to_vec(),
            errors: errors.iter().map(|e| e.to_string()).collect(),
        };
        let none = failed(&[], &[]);
        assert_eq!(status(Movies, "x", 5, &none, 2), "");
        assert_eq!(
            status(Movies, "x", 5, &failed(&["tpb"], &[]), 2),
            "tpb didn't answer."
        );
        assert_eq!(
            status(Movies, "x", 0, &failed(&["yts", "tpb"], &[]), 2),
            "yts and tpb didn't answer. Check your connection and try again."
        );
        assert_eq!(
            status(Anime, "sintel ", 0, &none, 1),
            "No anime results for \"sintel\". Tab searches movies and series."
        );
        assert_eq!(
            status(Series, "x", 0, &failed(&["tpb"], &[]), 2),
            "tpb didn't answer. No series results for \"x\". Tab searches anime and movies."
        );
        assert_eq!(
            status(Movies, "", 0, &none, 2),
            "No movies to show. Tab searches series and anime."
        );
        // A site's own error is shown as is, without blaming the connection.
        assert_eq!(
            status(
                Anime,
                "x",
                0,
                &failed(&[], &["nyaa user \"typo\" not found"]),
                1
            ),
            "nyaa user \"typo\" not found."
        );
        assert_eq!(
            status(
                Movies,
                "x",
                3,
                &failed(&["yts"], &["searching tpb: 429 Too Many Requests"]),
                2
            ),
            "yts didn't answer. searching tpb: 429 Too Many Requests."
        );
        assert_eq!(
            status(
                Movies,
                "x",
                0,
                &failed(&["yts"], &["parsing tpb results"]),
                2
            ),
            "yts didn't answer. parsing tpb results."
        );
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
