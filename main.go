// peerflix streams a torrent (magnet link, .torrent file or URL) to IINA.
//
// The selected file is served over a local HTTP server with range support;
// reads prioritize the pieces around the player's read position, so playback
// starts as soon as the first pieces arrive and seeking works.
package main

import (
	"context"
	"errors"
	"flag"
	"fmt"
	"io"
	"log/slog"
	"net"
	"net/http"
	"net/url"
	"os"
	"os/exec"
	"os/signal"
	"path"
	"path/filepath"
	"slices"
	"strings"
	"syscall"
	"time"

	"github.com/anacrolix/torrent"
	"github.com/anacrolix/torrent/metainfo"
)

var videoExts = []string{".mkv", ".mp4", ".avi", ".mov", ".webm", ".m4v", ".wmv", ".flv", ".ts", ".m2ts", ".mpg", ".mpeg"}

func main() {
	var (
		port    = flag.Int("port", 8888, "HTTP port to serve the stream on (0 = random)")
		dir     = flag.String("dir", "", "download directory (default: temporary dir, removed on exit)")
		index   = flag.Int("index", -1, "file index to stream (default: largest video file)")
		list    = flag.Bool("list", false, "list files in the torrent and exit")
		noPlay  = flag.Bool("no-play", false, "don't launch IINA, just serve the stream")
		user    = flag.String("user", "", "only search torrents from this nyaa uploader (name or profile URL)")
		trusted = flag.Bool("trusted", false, "only search torrents from trusted nyaa uploaders")
		print   = flag.Bool("print", false, "print nyaa results for the search terms and exit")
	)
	flag.Usage = func() {
		fmt.Fprintf(os.Stderr, "Usage:\n  %[1]s [flags] [search terms]                      search nyaa.si\n  %[1]s [flags] <magnet | file.torrent | http(s) url>\n\nFlags:\n", os.Args[0])
		flag.PrintDefaults()
	}
	flag.Parse()

	ctx, stop := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer stop()

	if *print {
		printResults(ctx, os.Stdout, strings.Join(flag.Args(), " "), nyaaUser(*user), *trusted)
		return
	}

	err := func() error {
		source := strings.Join(flag.Args(), " ")
		if !isTorrentSource(source) {
			var err error
			if source, err = searchInteractive(ctx, source, nyaaUser(*user), *trusted); err != nil {
				return err
			}
		}
		return run(ctx, source, options{
			port: *port, dir: *dir, index: *index,
			list: *list, noPlay: *noPlay,
		})
	}()
	if err != nil && !errors.Is(err, context.Canceled) && !errors.Is(err, errNoSelection) {
		fmt.Fprintln(os.Stderr, "error:", err)
		os.Exit(1)
	}
}

// nyaaUser accepts an uploader name or a profile URL such as
// https://nyaa.si/user/NAME and returns the name.
func nyaaUser(s string) string {
	s = strings.TrimRight(s, "/")
	if i := strings.LastIndex(s, "/user/"); i >= 0 {
		s = s[i+len("/user/"):]
	}
	return s
}

// isTorrentSource reports whether s is something addTorrent can load rather
// than search terms.
func isTorrentSource(s string) bool {
	for _, prefix := range []string{"magnet:", "http://", "https://"} {
		if strings.HasPrefix(s, prefix) {
			return true
		}
	}
	if !strings.HasSuffix(s, ".torrent") {
		return false
	}
	_, err := os.Stat(s)
	return err == nil
}

type options struct {
	port, index  int
	dir          string
	list, noPlay bool
}

func run(ctx context.Context, source string, opts options) error {
	dataDir := opts.dir
	if dataDir == "" {
		tmp, err := os.MkdirTemp("", "peerflix-")
		if err != nil {
			return err
		}
		dataDir = tmp
		defer os.RemoveAll(tmp)
	}

	cfg := torrent.NewDefaultClientConfig()
	cfg.DataDir = dataDir
	cfg.ListenPort = 0
	cfg.Slogger = slog.New(quietHandler{slog.NewTextHandler(clearLineWriter{os.Stderr}, &slog.HandlerOptions{Level: slog.LevelWarn})})
	client, err := torrent.NewClient(cfg)
	if err != nil {
		return fmt.Errorf("creating torrent client: %w", err)
	}
	defer client.Close()

	t, err := addTorrent(ctx, client, source)
	if err != nil {
		return err
	}

	fmt.Fprintln(os.Stderr, "Fetching torrent metadata...")
	select {
	case <-t.GotInfo():
	case <-ctx.Done():
		return ctx.Err()
	}

	files := t.Files()
	if opts.list {
		for i, f := range files {
			fmt.Printf("%3d  %9s  %s\n", i, humanBytes(f.Length()), f.DisplayPath())
		}
		return nil
	}

	file, err := pickFile(files, opts.index)
	if err != nil {
		return err
	}
	// Fetch the rest of the file in the background; readers still take priority.
	file.Download()

	ln, err := net.Listen("tcp", fmt.Sprintf("127.0.0.1:%d", opts.port))
	if err != nil {
		return fmt.Errorf("listening on port %d: %w", opts.port, err)
	}
	name := path.Base(file.DisplayPath())
	streamURL := fmt.Sprintf("http://%s/%s", ln.Addr(), url.PathEscape(name))

	srv := &http.Server{Handler: streamHandler(file, name)}
	go srv.Serve(ln)
	defer srv.Close()

	fmt.Fprintf(os.Stderr, "Streaming %s (%s)\n%s\n", name, humanBytes(file.Length()), streamURL)

	playerDone := make(chan error, 1)
	if !opts.noPlay {
		go func() { playerDone <- launchIINA(ctx, streamURL) }()
	}

	ticker := time.NewTicker(time.Second)
	defer ticker.Stop()
	var lastRead int64
	for {
		select {
		case <-ctx.Done():
			fmt.Fprintln(os.Stderr)
			return nil
		case err := <-playerDone:
			fmt.Fprintln(os.Stderr)
			return err
		case <-ticker.C:
			stats := t.Stats()
			read := stats.BytesReadUsefulData.Int64()
			fmt.Fprintf(os.Stderr, "\r\033[K%5.1f%%  %s/s  peers %d/%d  seeders %d",
				100*float64(file.BytesCompleted())/float64(max(file.Length(), 1)),
				humanBytes(read-lastRead), stats.ActivePeers, stats.TotalPeers, stats.ConnectedSeeders)
			lastRead = read
		}
	}
}

func addTorrent(ctx context.Context, client *torrent.Client, source string) (*torrent.Torrent, error) {
	switch {
	case strings.HasPrefix(source, "magnet:"):
		return client.AddMagnet(source)
	case strings.HasPrefix(source, "http://"), strings.HasPrefix(source, "https://"):
		req, err := http.NewRequestWithContext(ctx, http.MethodGet, source, nil)
		if err != nil {
			return nil, err
		}
		resp, err := http.DefaultClient.Do(req)
		if err != nil {
			return nil, fmt.Errorf("fetching torrent: %w", err)
		}
		defer resp.Body.Close()
		if resp.StatusCode != http.StatusOK {
			return nil, fmt.Errorf("fetching torrent: %s", resp.Status)
		}
		mi, err := metainfo.Load(resp.Body)
		if err != nil {
			return nil, fmt.Errorf("parsing torrent: %w", err)
		}
		return client.AddTorrent(mi)
	default:
		return client.AddTorrentFromFile(source)
	}
}

// pickFile returns the file at index, or the largest video file (falling back
// to the largest file) when index is negative.
func pickFile(files []*torrent.File, index int) (*torrent.File, error) {
	if index >= 0 {
		if index >= len(files) {
			return nil, fmt.Errorf("file index %d out of range (torrent has %d files)", index, len(files))
		}
		return files[index], nil
	}
	isVideo := func(f *torrent.File) bool {
		return slices.Contains(videoExts, strings.ToLower(filepath.Ext(f.DisplayPath())))
	}
	var best *torrent.File
	for _, f := range files {
		if best == nil ||
			(isVideo(f) && !isVideo(best)) ||
			(isVideo(f) == isVideo(best) && f.Length() > best.Length()) {
			best = f
		}
	}
	if best == nil {
		return nil, errors.New("torrent has no files")
	}
	return best, nil
}

func streamHandler(file *torrent.File, name string) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		reader := file.NewReader()
		defer reader.Close()
		reader.SetContext(r.Context())
		reader.SetResponsive()
		// The default readahead grows with the bytes read since the last
		// seek: seeks fetch just what's needed, steady playback buffers more.
		http.ServeContent(w, r, name, time.Time{}, reader.(io.ReadSeeker))
	})
}

// launchIINA opens the stream in IINA and returns once the player quits.
func launchIINA(ctx context.Context, streamURL string) error {
	bin, err := exec.LookPath("iina")
	if err != nil {
		bin = "/Applications/IINA.app/Contents/MacOS/iina-cli"
		if _, statErr := os.Stat(bin); statErr != nil {
			return errors.New("IINA not found; install it with `brew install --cask iina`")
		}
	}
	cmd := exec.CommandContext(ctx, bin, "--no-stdin", "--keep-running", streamURL)
	cmd.Stdout, cmd.Stderr = os.Stdout, os.Stderr
	return cmd.Run()
}

// quietHandler drops library log records caused by a cancelled context. The
// torrent reader logs every aborted read as an error, which happens routinely
// when the player closes a connection to seek.
type quietHandler struct{ slog.Handler }

func (h quietHandler) Handle(ctx context.Context, r slog.Record) error {
	canceled := false
	r.Attrs(func(a slog.Attr) bool {
		if err, ok := a.Value.Any().(error); ok && errors.Is(err, context.Canceled) {
			canceled = true
			return false
		}
		return true
	})
	if canceled {
		return nil
	}
	return h.Handler.Handle(ctx, r)
}

func (h quietHandler) WithAttrs(attrs []slog.Attr) slog.Handler {
	return quietHandler{h.Handler.WithAttrs(attrs)}
}

func (h quietHandler) WithGroup(name string) slog.Handler {
	return quietHandler{h.Handler.WithGroup(name)}
}

// clearLineWriter clears the status line before each write so log output
// doesn't get appended to it.
type clearLineWriter struct{ w io.Writer }

func (c clearLineWriter) Write(p []byte) (int, error) {
	if _, err := io.WriteString(c.w, "\r\033[K"); err != nil {
		return 0, err
	}
	return c.w.Write(p)
}

func humanBytes(n int64) string {
	const unit = 1024
	if n < unit {
		return fmt.Sprintf("%d B", n)
	}
	div, exp := int64(unit), 0
	for m := n / unit; m >= unit; m /= unit {
		div *= unit
		exp++
	}
	return fmt.Sprintf("%.1f %ciB", float64(n)/float64(div), "KMGTPE"[exp])
}
