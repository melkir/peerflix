package main

import (
	"context"
	"errors"
	"fmt"
	"io"
	"os"
	"os/exec"
	"strings"
)

var errNoSelection = errors.New("nothing selected")

// printResults writes nyaa's results for query as fzf input, one tab
// separated line per result: the torrent URL, the date, size and health, and
// the title. A failed search, such as nyaa being unavailable or rate
// limiting, prints nothing.
func printResults(ctx context.Context, w io.Writer, query, user string, trusted bool) {
	items, _ := searchNyaa(ctx, query, user, trusted)
	for _, it := range items {
		fmt.Fprintf(w, "%s\t\033[90m%s  %10s\033[0m  %s \t%s\n",
			it.Torrent, it.Date().Format("2006-01-02"), it.Size, health(it), it.Title)
	}
}

// health rates a torrent by its seeders relative to its leechers.
func health(it nyaaItem) string {
	switch {
	case it.Seeders == 0:
		return "\033[31m●\033[0m"
	case it.Seeders > it.Leechers:
		return "\033[32m●\033[0m"
	case it.Seeders == it.Leechers:
		return "\033[33m●\033[0m"
	default:
		return "\033[38;5;208m●\033[0m"
	}
}

// searchInteractive runs fzf over live nyaa searches, optionally restricted to
// one uploader or to trusted uploads, and returns the chosen torrent URL, or
// errNoSelection if the user quits.
func searchInteractive(ctx context.Context, initial, user string, trusted bool) (string, error) {
	self, err := os.Executable()
	if err != nil {
		return "", err
	}
	search := shellQuote(self) + " -print"
	if user != "" {
		search += " -user " + shellQuote(user)
	}
	if trusted {
		search += " -trusted"
	}
	search += " -- {q}"

	// fzf filters the current list on every keystroke, matching title terms
	// in nyaa's newest-first order, while a reload fetches nyaa's results for
	// the new query. fzf kills a running reload when the next one starts, so
	// the sleep debounces typing.
	cmd := exec.CommandContext(ctx, "fzf",
		"--ansi", "--exact", "-i", "--no-sort", "--tabstop", "1",
		"--query", initial,
		"--prompt", "nyaa> ",
		"--with-shell", "sh -c",
		"--delimiter", "\t", "--with-nth", "2..", "--nth", "2", "--accept-nth", "1",
		"--bind", "enter:accept-non-empty",
		"--bind", "start:reload:"+search,
		"--bind", "change:reload:sleep 0.25; "+search,
	)
	cmd.Stdin = os.Stdin
	cmd.Stderr = os.Stderr
	out, err := cmd.Output()
	var exitErr *exec.ExitError
	if errors.As(err, &exitErr) && (exitErr.ExitCode() == 1 || exitErr.ExitCode() == 130) {
		return "", errNoSelection // no match, or Esc/Ctrl-C
	}
	if err != nil {
		return "", fmt.Errorf("running fzf (0.60 or later is required): %w", err)
	}
	url := strings.TrimSpace(string(out))
	if url == "" {
		return "", errNoSelection
	}
	return url, nil
}

// shellQuote quotes s for sh.
func shellQuote(s string) string {
	return "'" + strings.ReplaceAll(s, "'", `'\''`) + "'"
}
