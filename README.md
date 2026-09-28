# peerflix

[![CI](https://github.com/melkir/peerflix/actions/workflows/ci.yml/badge.svg)](https://github.com/melkir/peerflix/actions/workflows/ci.yml)

Stream a torrent straight into [IINA](https://iina.io).

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
peerflix https://example.com/movie.torrent
```

## Search nyaa.si

```sh
peerflix                                  # type to search
peerflix big buck bunny                   # start with a query
peerflix --user NAME                      # browse/search one uploader
peerflix --user NAME QUERY
```

Search runs in [fzf](https://github.com/junegunn/fzf) 0.60 or later, which must be on your
`PATH`. Each keystroke instantly filters the loaded results by title (space separated terms, matches highlighted) while nyaa is re-queried in the background for the new query. Enter streams the selection to IINA, Esc quits. Each nyaa search returns its 75 newest matches, so type an episode number to reach older ones.

`--print` writes the results for the search terms as `URL<TAB>columns` lines, so you can pipe them into your own tools:

```sh
peerflix --print big buck bunny | fzf --ansi -d '\t' --with-nth 2.. --nth 2 --accept-nth 1 | xargs peerflix
```

Flags:

| flag | default | |
|---|---|---|
| `-i, --index N` | largest video | file to stream |
| `-l, --list` | | list files and exit |
| `-p, --port N` | 8888 | local HTTP port (0 = random) |
| `-d, --dir PATH` | temp dir | where to store data |
| `-n, --no-play` | | only serve `http://127.0.0.1:PORT/<name>` |
| `-t, --trusted` | | only search trusted nyaa uploads |
| `--print` | | print search results and exit |
| `-u, --user NAME` | | restrict search to a nyaa uploader (name or profile URL) |
| `-V, --version` | | print the version and exit |

peerflix exits when IINA quits (or on Ctrl-C) and removes the temp data unless `--dir` is set.
With `--dir`, data from an earlier run is checked and reused.

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
