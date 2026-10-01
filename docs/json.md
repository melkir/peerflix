# JSON

`peerflix --json` is how programs such as the IINA plugin search and stream through peerflix.

## Search

With search terms, it writes one line once every site has answered: `results`, each with its
`site`, `url` (magnet or .torrent), `title`, `date`, `size`, `seeders`, `leechers` and `info_hash`,
and a `summary` to show, such as the sites that didn't answer, or where else to look when nothing
was found.

```sh
peerflix --json -c movies tt1254207 | jq -r '.results[0].url' | xargs peerflix
```

## Stream

With a torrent, it writes the torrent's `info_hash`, its `files` (each an `index`, `path` and
`size`), its `episodes` (the indexes worth choosing between, in order) and a `control` URL, then
waits for the program there:

- `PUT control` streams the largest video, or `PUT control?index=N` file N, and answers with the
  stream's `name`, `url` and `subtitles` (each a `name` and `url`).
- `GET control` answers with the stream's status, as in
  `{"state":"downloading","downloaded":314572800,"size":1395864371,"download_speed":4718592,"peers":14,"seen":52,"text":" 22.5%     4.5 MiB/s  14 peers, 52 seen"}`:
  its `state` (`checking` data from an earlier run, `downloading`, `paused` or `done`), the video's
  bytes, the torrent's bytes per second and peers, and the line peerflix shows for it.
- `PUT control?pause` and `PUT control?resume` pause and resume the download once streaming; the
  player waits meanwhile.
- `DELETE control` stops peerflix.

peerflix also stops 10 minutes after listing the files if nothing was picked, and 30 seconds
