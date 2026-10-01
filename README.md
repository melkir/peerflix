# peerflix

[![CI](https://github.com/melkir/peerflix/actions/workflows/ci.yml/badge.svg)](https://github.com/melkir/peerflix/actions/workflows/ci.yml)

Search for anime, movies and series, and stream the torrent straight into [IINA](https://iina.io),
from the terminal, from IINA itself, or from a window of its own.

<img width="800" height="450" alt="peerflix demo: search, pick a torrent, and it plays in IINA" src="https://github.com/user-attachments/assets/e9a69810-ebc9-4a66-8468-88c7ca1a0085" />

<sub>The demo searches a local feed of Blender Studio open movies (CC BY) instead of the real sites.</sub>

## Install

```sh
mise use -g github:melkir/peerflix                                          # prebuilt Apple Silicon binary
cargo install --locked --git https://github.com/melkir/peerflix peerflix   # or build it
```

Search needs [fzf](https://github.com/junegunn/fzf) 0.60 or later on your `PATH`. For fish
completions, `ln -s (pwd)/completions/peerflix.fish ~/.config/fish/completions/` from a clone.

## Search

```sh
peerflix                           # browse the newest anime
peerflix frieren                   # search anime
peerflix -c movies sintel          # start in movies
peerflix -c series pioneer one s01 # a season, or S01E03, 1x03
peerflix -c movies tt1254207       # by IMDb ID (movies and series)
peerflix -u NAME                   # one nyaa uploader's anime (name or profile URL)
peerflix -t frieren                # trusted nyaa uploads only
```

Typing filters the loaded results at once while the sites are searched again for the new query.
**Tab** and **Shift-Tab** switch between the categories, keeping what you typed; **Enter** streams
the pick, **Esc** quits.

| category | sites |
|---|---|
| anime | nyaa's Anime category, its 75 newest matches |
| movies | YTS, and The Pirate Bay's SD, HD and 4K movies |
| series | EZTV, looked up through IMDb, and The Pirate Bay's SD, HD and 4K TV shows |

Each result shows its date, size and seeders, green when they outnumber leechers, yellow when
even, orange when fewer. Dead torrents are left out, as are cams, telesyncs and screeners among
movies, and a torrent found on both sites of a category is listed once. The sites are searched in parallel and each
one's results show as soon as it answers; one that doesn't answer in 8 seconds is named above the
results.

## Stream

```sh
peerflix 'magnet:?xt=urn:btih:...'
peerflix movie.torrent
peerflix https://webtorrent.io/torrents/sintel.torrent
```

When the torrent holds several episodes, such as a season pack, fzf lists them to pick one. The
subtitles that come with the video (`.srt`, `.ass`, `.ssa`, `.vtt`) are loaded in IINA too. While
it plays, one line shows the video's progress, the download speed and the peers.

The data stays in `$TMPDIR/peerflix` (or `--dir`), so playing the same torrent again reuses what
was downloaded. Files are named `NAME.part` until they're complete. peerflix exits when IINA quits,
or on Ctrl-C, which closes IINA too.

To try it, [WebTorrent's free torrents](https://webtorrent.io/free-torrents) are open movies such as
Sintel and Big Buck Bunny.

| flag | |
|---|---|
| `-c, --category NAME` | category to start searching in: `anime` (default), `movies` or `series` |
| `-u, --user NAME` | only search this nyaa uploader's anime |
| `-t, --trusted` | only search trusted nyaa uploads |
| `-i, --index N` | file to stream, instead of picking the episode in fzf |
| `-d, --dir PATH` | where to keep downloads (default `$TMPDIR/peerflix`) |
| `-p, --port N` | local HTTP port (default 8888, or a free one if taken; 0 = random) |
| `-n, --no-play` | only serve `http://127.0.0.1:PORT/<name>`, without launching IINA |
| `--no-upnp` | don't ask the router to forward the torrent port |
| `--json` | print JSON for programs, see below |

## IINA plugin

**Plugin › Search Torrents…** opens the same search in a window: type, switch category, pick a
torrent (and an episode when it holds several), and it plays in a new player with its subtitles.
**Up** and **Down** move through the list, **Enter** opens the pick, **Tab** and **Shift-Tab** switch
category, and **Esc** goes back from the episodes. The player shows the download's progress, speed
and peers in a corner until the file is downloaded (**Plugin › Show Download Status** hides it), and
so does a bar at the bottom of the search window for every stream playing, with buttons to pause
the download and to stop the stream. **Plugin › Pause Download** in the player pauses it too.
Closing the player, or stopping the stream, stops the other, and playing another episode of a
torrent that's playing replaces its stream.

To install it, enter `melkir/peerflix` under **Settings › Plugins › Install from GitHub…** in IINA;
doing it again updates it. It runs the peerflix of the same release, which it looks for in the
`PATH`, `~/.cargo/bin`, `/opt/homebrew/bin`, `/usr/local/bin`, then mise's shims, or where its
preferences say, and says so when the one it finds is of another version.

To work on it, link the folder into IINA's plugins, and restart IINA after changes:

```sh
ln -s (pwd)/iina-plugin ~/Library/Application\ Support/com.colliderli.iina/plugins/peerflix.iinaplugin-dev
```

## App

peerflix.app is the same search in a window, built with [GPUI Kit](https://gpui-kit.com): type to
search, **Tab** and **Shift-Tab** switch category, the arrows move through the results, and
**Enter** or a double click opens one. When the torrent holds several episodes, they're listed to
pick one (**Esc** goes back), and a torrent that's slow to answer says so. Each stream plays in an
IINA of its own, with a row at the bottom of the window showing its progress, speed and peers, and
buttons to pause its download and to stop it. A stream stops when its player quits, with its button,
or when the app quits, which closes its player and stops the download; playing the torrent again
resumes it. A player left open without its window stops after 30 seconds without playing. Playing
another episode of a torrent that's playing replaces its stream.

Download `peerflix-app-vX.Y.Z.zip` from the [latest release](https://github.com/melkir/peerflix/releases/latest)
(Apple Silicon) and move `peerflix.app` to `/Applications`. It isn't notarized, so macOS blocks it
at first: run `xattr -dr com.apple.quarantine /Applications/peerflix.app` once. To build it instead,
`mise run app` writes `target/app/peerflix.app`.

## JSON

`--json` is how programs such as the plugin search and stream through peerflix.

With search terms, it writes one line once every site has answered: `results`, each with its
`site`, `url` (magnet or .torrent), `title`, `date`, `size`, `seeders`, `leechers` and `info_hash`,
and a `summary` to show, such as the sites that didn't answer, or where else to look when nothing
was found.

```sh
peerflix --json -c movies tt1254207 | jq -r '.results[0].url' | xargs peerflix
```

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
after the last player disconnects.

`PEERFLIX_NYAA_URL`, `PEERFLIX_YTS_URL`, `PEERFLIX_EZTV_URL`, `PEERFLIX_TPB_URL` and
`PEERFLIX_IMDB_URL` point search at other hosts, such as a mirror when a site moves, or a mock.

## Development

[mise](https://mise.jdx.dev) installs the pinned toolchain and runs the same checks as CI:

```sh
mise install
mise run ci           # fmt check, clippy, test and build
mise run app          # build target/app/peerflix.app
```

The workspace holds three crates: `peerflix-core`, the searching and streaming both front ends use,
`peerflix`, the command line, and `peerflix-app`, the window. Building the app needs Xcode, for
GPUI's Metal shaders.

To release, run [cargo-release](https://github.com/crate-ci/cargo-release) on `main`: it bumps the
version (the plugin's too), tags `vX.Y.Z` and pushes, and the tag has GitHub Actions publish the
binary, the app and the plugin.

```sh
cargo release minor            # dry run
cargo release minor --execute
```
