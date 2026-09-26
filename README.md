# peerflix

Stream a torrent straight into [IINA](https://iina.io).

```sh
go build -o peerflix .
./peerflix
./peerflix 'magnet:?xt=urn:btih:...'
./peerflix movie.torrent
./peerflix https://example.com/movie.torrent
```

## Install

```sh
go install .                                # builds ~/go/bin/peerflix
ln -s (pwd)/completions/peerflix.fish ~/.config/fish/completions/   # fish completions
```

## Search nyaa.si

```sh
./peerflix                                  # type to search
./peerflix big buck bunny                   # start with a query
./peerflix -user NAME                       # browse/search one uploader
./peerflix -user NAME QUERY
```

Search runs in [fzf](https://github.com/junegunn/fzf) 0.60 or later, which must be on your
`PATH`. Each keystroke instantly filters the loaded results by title (space separated terms, matches highlighted) while nyaa is re-queried in the background for the new query. Enter streams the selection to IINA, Esc quits. Each nyaa search returns its 75 newest matches, so type an episode number to reach older ones.

`-print` writes the results for the search terms as `URL<TAB>columns` lines, so you can pipe them into your own tools:

```sh
./peerflix -print big buck bunny | fzf --ansi -d '\t' --with-nth 2.. --nth 2 --accept-nth 1 | xargs ./peerflix
```

Flags:

| flag | default | |
|---|---|---|
| `-index N` | largest video | file to stream |
| `-list` | | list files and exit |
| `-port N` | 8888 | local HTTP port (0 = random) |
| `-dir PATH` | temp dir | where to store data |
| `-keep` | false | keep data on exit |
| `-no-play` | false | only serve `http://127.0.0.1:PORT/<name>` |
| `-trusted` | false | only search trusted nyaa uploads |
| `-print` | | print search results and exit |
| `-user NAME` | | restrict search to a nyaa uploader (name or profile URL) |
| `-readahead N` | 32 MiB | bytes prioritized ahead of the play head |

peerflix exits when IINA quits (or on Ctrl-C) and removes the temp data unless `-keep` is set.
