//! Picking from lists in fzf: the live search and the episode picker.

use std::process::{Command, Stdio};

use anyhow::Context;
use tokio::io::AsyncWriteExt;

use crate::search::Category;

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
}
