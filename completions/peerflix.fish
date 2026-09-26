# fish completions for peerflix. Install with:
#   ln -s (pwd)/completions/peerflix.fish ~/.config/fish/completions/

complete -c peerflix -a '(__fish_complete_suffix .torrent)'

complete -c peerflix -o dir -x -a '(__fish_complete_directories)' -d 'Download directory'
complete -c peerflix -o index -x -d 'File index to stream'
complete -c peerflix -o list -d 'List files in the torrent and exit'
complete -c peerflix -o no-play -d "Don't launch IINA, just serve the stream"
complete -c peerflix -o port -x -d 'HTTP port to serve the stream on'
complete -c peerflix -o print -d 'Print nyaa results and exit'
complete -c peerflix -o trusted -d 'Only search trusted nyaa uploaders'
complete -c peerflix -o user -x -d 'Only search this nyaa uploader'
complete -c peerflix -o keep -d 'Keep downloaded data on exit'
complete -c peerflix -o readahead -x -d 'Bytes to prioritize ahead of the read position'
