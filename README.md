# peerflix

Stream a torrent straight into [IINA](https://iina.io).

```sh
go build -o peerflix .
./peerflix 'magnet:?xt=urn:btih:...'
./peerflix movie.torrent
./peerflix https://example.com/movie.torrent
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
| `-readahead N` | 32 MiB | bytes prioritized ahead of the play head |

peerflix exits when IINA quits (or on Ctrl-C) and removes the temp data unless `-keep` is set.
