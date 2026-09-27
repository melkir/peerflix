package main

import (
	"bytes"
	"context"
	"errors"
	"fmt"
	"io"
	"log/slog"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"testing"

	"github.com/anacrolix/torrent"
	"github.com/anacrolix/torrent/bencode"
	"github.com/anacrolix/torrent/metainfo"
)

func TestNyaaUser(t *testing.T) {
	tests := map[string]string{
		"":                              "",
		"someone":                       "someone",
		"https://nyaa.si/user/someone":  "someone",
		"https://nyaa.si/user/someone/": "someone",
		"nyaa.si/user/someone":          "someone",
	}
	for in, want := range tests {
		if got := nyaaUser(in); got != want {
			t.Errorf("nyaaUser(%q) = %q, want %q", in, got, want)
		}
	}
}

func TestIsTorrentSource(t *testing.T) {
	existing := filepath.Join(t.TempDir(), "movie.torrent")
	if err := os.WriteFile(existing, nil, 0o644); err != nil {
		t.Fatal(err)
	}
	tests := map[string]bool{
		"magnet:?xt=urn:btih:abc":           true,
		"http://example.com/a.torrent":      true,
		"https://example.com/a":             true,
		existing:                            true,
		"missing.torrent":                   false,
		"big buck bunny":                    false,
		"":                                  false,
		filepath.Join(t.TempDir(), "x.mkv"): false,
	}
	for in, want := range tests {
		if got := isTorrentSource(in); got != want {
			t.Errorf("isTorrentSource(%q) = %v, want %v", in, got, want)
		}
	}
}

func TestHumanBytes(t *testing.T) {
	tests := map[int64]string{
		0:       "0 B",
		1023:    "1023 B",
		1024:    "1.0 KiB",
		1536:    "1.5 KiB",
		1 << 20: "1.0 MiB",
		5 << 30: "5.0 GiB",
		3 << 40: "3.0 TiB",
	}
	for in, want := range tests {
		if got := humanBytes(in); got != want {
			t.Errorf("humanBytes(%d) = %q, want %q", in, got, want)
		}
	}
}

func TestQuietHandlerDropsCanceled(t *testing.T) {
	var buf bytes.Buffer
	log := slog.New(quietHandler{slog.NewTextHandler(&buf, nil)}).With("k", "v")
	log.Error("read failed", "err", fmt.Errorf("reading: %w", context.Canceled))
	if buf.Len() != 0 {
		t.Errorf("canceled record was logged: %q", buf.String())
	}
	log.Error("read failed", "err", errors.New("boom"))
	if !strings.Contains(buf.String(), "boom") || !strings.Contains(buf.String(), "k=v") {
		t.Errorf("other record not logged with attrs: %q", buf.String())
	}
}

func TestClearLineWriter(t *testing.T) {
	var buf bytes.Buffer
	n, err := clearLineWriter{&buf}.Write([]byte("hello\n"))
	if err != nil || n != 6 {
		t.Fatalf("Write = %d, %v", n, err)
	}
	if got, want := buf.String(), "\r\033[Khello\n"; got != want {
		t.Errorf("got %q, want %q", got, want)
	}
}

// newLocalTorrent seeds a torrent with the given files from a temp dir, with
// all networking disabled, and returns it once its data is verified.
func newLocalTorrent(t *testing.T, files map[string]string) *torrent.Torrent {
	t.Helper()
	dir := t.TempDir()
	root := filepath.Join(dir, "content")
	for name, data := range files {
		p := filepath.Join(root, name)
		if err := os.MkdirAll(filepath.Dir(p), 0o755); err != nil {
			t.Fatal(err)
		}
		if err := os.WriteFile(p, []byte(data), 0o644); err != nil {
			t.Fatal(err)
		}
	}
	info := metainfo.Info{PieceLength: 16 << 10}
	if err := info.BuildFromFilePath(root); err != nil {
		t.Fatal(err)
	}
	var mi metainfo.MetaInfo
	var err error
	if mi.InfoBytes, err = bencode.Marshal(info); err != nil {
		t.Fatal(err)
	}

	cfg := torrent.NewDefaultClientConfig()
	cfg.DataDir = dir
	cfg.NoDHT = true
	cfg.DisableTrackers = true
	cfg.DisableTCP = true
	cfg.DisableUTP = true
	cfg.DisableWebtorrent = true
	cfg.DisableWebseeds = true
	cfg.NoDefaultPortForwarding = true
	cfg.ListenPort = 0
	cfg.Slogger = slog.New(slog.NewTextHandler(io.Discard, nil))
	client, err := torrent.NewClient(cfg)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { client.Close() })
	tor, err := client.AddTorrent(&mi)
	if err != nil {
		t.Fatal(err)
	}
	<-tor.GotInfo()
	for i := range tor.NumPieces() {
		if err := tor.Piece(i).VerifyDataContext(t.Context()); err != nil {
			t.Fatal(err)
		}
	}
	return tor
}

func TestPickFile(t *testing.T) {
	tor := newLocalTorrent(t, map[string]string{
		"a/sample.mkv":  strings.Repeat("s", 10),
		"a/movie.mp4":   strings.Repeat("m", 100),
		"a/extras.zip":  strings.Repeat("z", 1000),
		"a/subs/en.srt": "subs",
	})
	files := tor.Files()
	name := func(f *torrent.File) string { return filepath.Base(f.DisplayPath()) }

	f, err := pickFile(files, -1)
	if err != nil || name(f) != "movie.mp4" {
		t.Errorf("pickFile(-1) = %v, %v; want the largest video, movie.mp4", f, err)
	}
	for i, want := range files {
		if f, err := pickFile(files, i); err != nil || f != want {
			t.Errorf("pickFile(%d) = %v, %v; want %v", i, f, err, want)
		}
	}
	if _, err := pickFile(files, len(files)); err == nil {
		t.Error("pickFile with an out of range index succeeded")
	}
	if _, err := pickFile(nil, -1); err == nil {
		t.Error("pickFile with no files succeeded")
	}
}

func TestPickFileFallsBackToLargest(t *testing.T) {
	tor := newLocalTorrent(t, map[string]string{
		"b/small.txt": "x",
		"b/big.bin":   strings.Repeat("b", 50),
	})
	f, err := pickFile(tor.Files(), -1)
	if err != nil || filepath.Base(f.DisplayPath()) != "big.bin" {
		t.Errorf("pickFile(-1) = %v, %v; want big.bin", f, err)
	}
}

func TestStreamHandler(t *testing.T) {
	data := strings.Repeat("0123456789", 5000) // spans several pieces
	tor := newLocalTorrent(t, map[string]string{"c/video.mkv": data, "c/other.txt": "x"})
	file, err := pickFile(tor.Files(), -1)
	if err != nil {
		t.Fatal(err)
	}
	srv := httptest.NewServer(streamHandler(file, "video.mkv"))
	defer srv.Close()

	get := func(rangeHeader string) (*http.Response, string) {
		t.Helper()
		req, _ := http.NewRequestWithContext(t.Context(), http.MethodGet, srv.URL+"/video.mkv", nil)
		if rangeHeader != "" {
			req.Header.Set("Range", rangeHeader)
		}
		resp, err := http.DefaultClient.Do(req)
		if err != nil {
			t.Fatal(err)
		}
		defer resp.Body.Close()
		body, err := io.ReadAll(resp.Body)
		if err != nil {
			t.Fatal(err)
		}
		return resp, string(body)
	}

	resp, body := get("")
	if resp.StatusCode != http.StatusOK || body != data {
		t.Errorf("full GET: status %d, %d bytes; want 200, %d bytes", resp.StatusCode, len(body), len(data))
	}
	if ct := resp.Header.Get("Content-Type"); ct == "" {
		t.Error("no Content-Type")
	}

	resp, body = get("bytes=20000-20009")
	if resp.StatusCode != http.StatusPartialContent || body != data[20000:20010] {
		t.Errorf("range GET: status %d, body %q; want 206, %q", resp.StatusCode, body, data[20000:20010])
	}
}
