# peerflix

![CI](https://github.com/melkir/peerflix/actions/workflows/ci.yml/badge.svg)

Search for anime, movies and series, and stream the torrent straight into [IINA](https://iina.io),
from the terminal, from IINA itself, or from a window of its own.

![peerflix demo: search, pick a torrent, and it plays in IINA](https://github.com/user-attachments/assets/e9a69810-ebc9-4a66-8468-88c7ca1a0085)

The demo searches a local feed of Blender Studio open movies (CC BY) instead of the real sites.

## Install

```sh
mise use -g github:melkir/peerflix                                          # prebuilt Apple Silicon binary
cargo install --locked --git https://github.com/melkir/peerflix peerflix   # or build it
```

Search needs [fzf](https://github.com/junegunn/fzf) 0.60 or later. For fish completions,
`ln -s (pwd)/completions/peerflix.fish ~/.config/fish/completions/` from a clone.

- **IINA plugin**: enter `melkir/peerflix` under **Settings › Plugins › Install from GitHub…**, then
use **Plugin › Search Torrents…**. It needs the command line installed.
- **App**: download `peerflix-app-vX.Y.Z.zip` from the [latest release](https://github.com/melkir/peerflix/releases/latest),  
move `Peerflix.app` to `/Applications`, and since it isn't notarized, run  
`xattr -dr com.apple.quarantine /Applications/Peerflix.app` once.

## Usage

```sh
peerflix                           # browse the newest anime
peerflix -c anime frieren          # search anime
peerflix -c movies sintel          # search movies, or by IMDb ID: tt1254207
peerflix -c series pioneer one s01 # a season, or S01E03, 1x03
peerflix -u NAME                   # one nyaa uploader's anime (name or profile URL)
peerflix -t frieren                # trusted nyaa uploads only
peerflix https://webtorrent.io/torrents/sintel.torrent  # stream a URL, .torrent file or magnet
```

**Tab** and **Shift-Tab** switch category, **Enter** streams the pick, **Esc** quits. Once an episode
of a season is closed, its episodes come back with it selected. Downloads stay
in `$TMPDIR/peerflix` (or `--dir`), so playing a torrent again reuses them. See
`peerflix --help` for the options, and [docs/json.md](docs/json.md) for driving it from a program.

## Development

[mise](https://mise.jdx.dev) installs the pinned toolchain and runs the same checks as CI:

```sh
mise install
mise run ci           # fmt check, clippy, test and build
mise run app          # build target/app/Peerflix.app (needs Xcode)
```

To work on the plugin, link it into IINA and restart IINA after changes:

```sh
ln -s (pwd)/iina-plugin ~/Library/Application\ Support/com.colliderli.iina/plugins/peerflix.iinaplugin-dev
```

`PEERFLIX_NYAA_URL`, `PEERFLIX_YTS_URL`, `PEERFLIX_EZTV_URL`, `PEERFLIX_TPB_URL` and
`PEERFLIX_IMDB_URL` point search at other hosts, such as a mirror or a mock.

To release, run `cargo release minor --execute` on `main` (without `--execute` for a dry run): it
bumps the versions, tags `vX.Y.Z` and pushes, and GitHub Actions publishes the binary, the app and
the plugin.