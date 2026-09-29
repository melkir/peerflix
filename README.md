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
peerflix                                  # type to search anime
peerflix big buck bunny                   # start with a query
peerflix -c movies sintel                 # start in movies
peerflix -c series pioneer one s01        # one season (or S01E03, 1x03) of a show
peerflix --user NAME                      # browse/search one nyaa uploader's anime
peerflix --user NAME QUERY
```

Search has three categories, and Tab (or Shift-Tab to go back) switches between them, keeping
what you typed. The prompt shows the current one:

- **anime**: nyaa's Anime category, the 75 newest matches, so type an episode number to reach
  older ones.
- **movies**: YTS, one line per movie and quality, and The Pirate Bay's SD, HD and 4K movies
  through its apibay API, both most seeded first. Cams, telesyncs and screeners are left out.
- **series**: EZTV and The Pirate Bay's SD, HD and 4K TV shows. EZTV only looks up shows by IMDb
  ID, so the query goes through IMDb's title suggestions first and the best matching show is
  listed, newest first. A trailing `S02`, `S02E03` or `2x03` narrows EZTV's
  results to that season or episode.

An IMDb ID instead of a title, such as `tt1254207` or `tt1748166 S02`, looks the movie or show up
by ID on YTS, EZTV and The Pirate Bay. nyaa has no IMDb IDs.

Before you type anything, anime lists nyaa's newest uploads, movies The Pirate Bay's top 100 HD
movies, and series its top 100 HD TV shows.

Each result shows its date, size and number of seeders, and in movies and series the site it
came from. The seeder count is green when seeders outnumber leechers, yellow when they're even,
and orange when leechers outnumber them.
Dead torrents, which have no seeders, are left out. When a search finds nothing, or a site
doesn't answer, a line above the results says so. The sites of a category are queried in parallel,
and each one's results show up as soon as it answers, so a slow or unreachable site (which gets 8
seconds) never holds up the other. A torrent the other site already listed isn't shown twice.

Search runs in [fzf](https://github.com/junegunn/fzf) 0.60 or later, which must be on your
`PATH`. Each keystroke instantly filters the loaded results by title (space separated terms, matches highlighted) while the sites are re-queried in the background for the new query. Enter streams the selection to IINA, Esc quits.

`--print` writes the results for the search terms (with `-c`, for that category) as `URL<TAB>columns` lines, so you can pipe them into your own tools:

```sh
peerflix --print big buck bunny | fzf --ansi -d '\t' --with-nth 2.. --nth 2 --accept-nth 1 | xargs peerflix
```

`--json` writes them as one JSON object once every site has answered, for programs that search
through peerflix: `results`, each with its `site`, `url` (magnet or .torrent), `title`, `date`,
`size`, `seeders`, `leechers` and `info_hash`, then the sites that failed, as `unanswered` (names)
and `errors` (messages).

```sh
peerflix --json -c movies tt1254207 | jq -r '.results[0].url' | xargs peerflix
```

Flags:

| flag | default | |
|---|---|---|
| `-i, --index N` | ask, or largest video | file to stream (see `--list`) |
| `-l, --list` | | list files and exit |
| `-p, --port N` | 8888 | local HTTP port (0 = random); a random one if 8888 is taken |
| `-d, --dir PATH` | `$TMPDIR/peerflix` | where to store data |
| `--no-upnp` | | don't ask the router to forward the torrent port |
| `-n, --no-play` | | only serve `http://127.0.0.1:PORT/<name>` |
| `-c, --category NAME` | anime | category to start searching in: `anime`, `movies` or `series` |
| `-t, --trusted` | | only search trusted nyaa uploads (anime) |
| `--print` | | print search results and exit |
| `--json` | | print search results as JSON and exit |
| `-u, --user NAME` | | restrict anime search to a nyaa uploader, name or profile URL |
| `-V, --version` | | print the version and exit |

`PEERFLIX_NYAA_URL`, `PEERFLIX_YTS_URL`, `PEERFLIX_EZTV_URL`, `PEERFLIX_TPB_URL` and `PEERFLIX_IMDB_URL` point search
at other hosts, such as a mirror when a site moves, or a local mock.

peerflix exits when IINA quits (or on Ctrl-C). Downloaded data stays in the data directory, so
playing the same torrent again checks and reuses it instead of downloading it again. Only the files
being downloaded are created there, named `NAME.part` until they're complete; a neighbouring file
that shares a piece with them can also be left as a small `.part`. macOS clears `$TMPDIR` of files
unused for a few days; use `--dir` to keep data somewhere else.

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
