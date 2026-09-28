# peerflix

[![CI](https://github.com/melkir/peerflix/actions/workflows/ci.yml/badge.svg)](https://github.com/melkir/peerflix/actions/workflows/ci.yml)

Stream a torrent straight into [IINA](https://iina.io).

<img width="800" height="450" alt="peerflix demo: search, pick a torrent, and it plays in IINA" src="https://github.com/user-attachments/assets/e9a69810-ebc9-4a66-8468-88c7ca1a0085" />

<sub>The demo searches a local feed of Blender Studio open movies (CC BY) instead of the real sites.</sub>

## Install

```sh
mise use -g github:melkir/peerflix                                 # prebuilt Apple Silicon binary
cargo install --locked --git https://github.com/melkir/peerflix   # builds ~/.cargo/bin/peerflix
```

[mise](https://mise.jdx.dev) fetches the binary from [Releases](https://github.com/melkir/peerflix/releases)
and `mise upgrade` keeps it current; you can also download it from there by hand.

From a clone, `cargo install --locked --path .` does the same, and
`ln -s (pwd)/completions/peerflix.fish ~/.config/fish/completions/` adds fish completions.
To try it without installing, `cargo run --release -- <args>`.

## Usage

```sh
peerflix 'magnet:?xt=urn:btih:...'
peerflix movie.torrent
peerflix https://webtorrent.io/torrents/sintel.torrent
```

When a torrent holds several episodes, such as a season pack, fzf lists them in order to pick
one; `--index` skips the question, as does running without a terminal, which streams the largest
video.

Subtitle files (`.srt`, `.ass`, `.ssa`, `.vtt`) that come with the video in the torrent are
downloaded along with it and loaded in IINA: those named after the episode
(`Show.S01E03.en.srt`), in a folder named after it (`Subs/Show.S01E03/`) or tagged with the same
`S01E03`, or all of them when the torrent holds a single video.

To try it out, [WebTorrent's free torrents](https://webtorrent.io/free-torrents) are open movies
such as Sintel and Big Buck Bunny.

## Search

```sh
peerflix                                  # type to search
peerflix big buck bunny                   # start with a query
peerflix -s yts inception                 # search only some sites
peerflix house of the dragon s02          # one season (or S02E03, 2x03) of a show
peerflix --user NAME                      # browse/search one nyaa uploader
peerflix --user NAME QUERY
```

Search covers four sites at once, and each result is tagged with the site it came from:

- [nyaa.si](https://nyaa.si), mostly anime: the 75 newest matches, so type an episode number to reach older ones.
- [YTS](https://yts.bz) movies: one line per movie and quality, most seeded first.
- [EZTV](https://eztvx.to) TV shows: EZTV only looks up shows by IMDb ID, so the query goes through
  IMDb's title suggestions first and the best matching show is listed, newest first. A trailing
  `S02`, `S02E03` or `2x03` narrows it to that season or episode.
- [The Pirate Bay](https://thepiratebay.org) video torrents, through its [apibay](https://apibay.org)
  API: up to 100 matches, most seeded first.

A torrent that another site already listed isn't shown twice.

The sites are queried in parallel, and each one's results show up as soon as it answers, so a
slow or unreachable site (which gets 8 seconds) never holds up the others.

Search runs in [fzf](https://github.com/junegunn/fzf) 0.60 or later, which must be on your
`PATH`. Each keystroke instantly filters the loaded results by title (space separated terms, matches highlighted) while the sites are re-queried in the background for the new query. Enter streams the selection to IINA, Esc quits.

`--print` writes the results for the search terms as `URL<TAB>columns` lines, so you can pipe them into your own tools:

```sh
peerflix --print big buck bunny | fzf --ansi -d '\t' --with-nth 2.. --nth 2 --accept-nth 1 | xargs peerflix
```

Flags:

| flag | default | |
|---|---|---|
| `-i, --index N` | ask, or largest video | file to stream (see `--list`) |
| `-l, --list` | | list files and exit |
| `-p, --port N` | 8888 | local HTTP port (0 = random) |
| `-d, --dir PATH` | `$TMPDIR/peerflix` | where to store data |
| `-n, --no-play` | | only serve `http://127.0.0.1:PORT/<name>` |
| `-s, --source LIST` | all | sites to search: `nyaa`, `yts`, `eztv`, `tpb`, comma separated |
| `-t, --trusted` | | only search trusted nyaa uploads (implies `-s nyaa`) |
| `--print` | | print search results and exit |
| `-u, --user NAME` | | restrict search to a nyaa uploader, name or profile URL (implies `-s nyaa`) |
| `-V, --version` | | print the version and exit |

`PEERFLIX_NYAA_URL`, `PEERFLIX_YTS_URL`, `PEERFLIX_EZTV_URL`, `PEERFLIX_TPB_URL` and `PEERFLIX_IMDB_URL` point search
at other hosts, such as a mirror when a site moves, or a local mock.

peerflix exits when IINA quits (or on Ctrl-C). Downloaded data stays in the data directory, so
playing the same torrent again checks and reuses it instead of downloading it again. macOS clears
`$TMPDIR` of files unused for a few days; use `--dir` to keep data somewhere else.

## Development

[mise](https://mise.jdx.dev) installs the pinned Rust toolchain and cargo-release and runs the
same tasks as CI:

```sh
mise install          # install the tools
mise run ci           # fmt check, clippy, test and build
```

To release, run [cargo-release](https://github.com/crate-ci/cargo-release) on `main`. It bumps
the version in `Cargo.toml`, commits, tags `vX.Y.Z` and pushes; the tag makes GitHub Actions
build the Apple Silicon binary and publish the release:

```sh
cargo release patch            # dry run
cargo release patch --execute
```
