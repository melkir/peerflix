# fish completions for peerflix. Install with:
#   ln -s (pwd)/completions/peerflix.fish ~/.config/fish/completions/

complete -c peerflix -a '(__fish_complete_suffix .torrent)'

complete -c peerflix -s d -l dir -x -a '(__fish_complete_directories)' -d 'Download directory'
complete -c peerflix -s i -l index -x -d 'File index to stream'
complete -c peerflix -s l -l list -d 'List files in the torrent and exit'
complete -c peerflix -s n -l no-play -d "Don't launch IINA, just serve the stream"
complete -c peerflix -s p -l port -x -d 'HTTP port to serve the stream on'
complete -c peerflix -l no-upnp -d "Don't ask the router to forward the torrent port"
complete -c peerflix -l print -d 'Print search results and exit'
complete -c peerflix -s c -l category -x -a 'anime movies series' -d 'Category to start searching in'
complete -c peerflix -s t -l trusted -d 'Only search trusted nyaa uploaders'
complete -c peerflix -s u -l user -x -d 'Only search this nyaa uploader'
complete -c peerflix -s h -l help -d 'Print help'
complete -c peerflix -s V -l version -d 'Print version'
