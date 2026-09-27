package main

import (
	"bytes"
	"fmt"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
	"time"
)

const sampleFeed = `<?xml version="1.0" encoding="utf-8"?>
<rss xmlns:atom="http://www.w3.org/2005/Atom" xmlns:nyaa="https://nyaa.si/xmlns/nyaa" version="2.0">
  <channel>
    <title>Nyaa - Torrent File RSS</title>
    <item>
      <title>[Group] Big Buck Bunny - 01 [1080p].mkv</title>
      <link>https://nyaa.si/download/1.torrent</link>
      <guid isPermaLink="true">https://nyaa.si/view/1</guid>
      <pubDate>Sat, 26 Sep 2026 12:00:00 -0000</pubDate>
      <nyaa:seeders>42</nyaa:seeders>
      <nyaa:leechers>3</nyaa:leechers>
      <nyaa:infoHash>0123456789abcdef0123456789abcdef01234567</nyaa:infoHash>
      <nyaa:category>Anime - English-translated</nyaa:category>
      <nyaa:size>1.2 GiB</nyaa:size>
    </item>
    <item>
      <title>Dead torrent</title>
      <link>https://nyaa.si/download/2.torrent</link>
      <guid isPermaLink="true">https://nyaa.si/view/2</guid>
      <pubDate>Fri, 25 Sep 2026 08:30:00 -0000</pubDate>
      <nyaa:seeders>0</nyaa:seeders>
      <nyaa:leechers>0</nyaa:leechers>
      <nyaa:size>300.0 MiB</nyaa:size>
    </item>
  </channel>
</rss>`

// fakeNyaa points nyaaURL at a test server for the duration of the test and
// returns the query of each request it receives.
func fakeNyaa(t *testing.T, handler http.HandlerFunc) *[]map[string]string {
	t.Helper()
	var queries []map[string]string
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		q := map[string]string{}
		for k := range r.URL.Query() {
			q[k] = r.URL.Query().Get(k)
		}
		queries = append(queries, q)
		handler(w, r)
	}))
	t.Cleanup(srv.Close)
	old := nyaaURL
	nyaaURL = srv.URL
	t.Cleanup(func() { nyaaURL = old })
	return &queries
}

func TestSearchNyaa(t *testing.T) {
	queries := fakeNyaa(t, func(w http.ResponseWriter, r *http.Request) {
		fmt.Fprint(w, sampleFeed)
	})
	items, err := searchNyaa(t.Context(), "big buck bunny", "someone", true)
	if err != nil {
		t.Fatal(err)
	}
	if len(items) != 2 {
		t.Fatalf("got %d items, want 2", len(items))
	}
	it := items[0]
	if it.Title != "[Group] Big Buck Bunny - 01 [1080p].mkv" || it.Torrent != "https://nyaa.si/download/1.torrent" ||
		it.View != "https://nyaa.si/view/1" || it.Seeders != 42 || it.Leechers != 3 ||
		it.InfoHash != "0123456789abcdef0123456789abcdef01234567" || it.Size != "1.2 GiB" {
		t.Errorf("unexpected first item: %+v", it)
	}
	if want := time.Date(2026, 9, 26, 12, 0, 0, 0, time.UTC); !it.Date().Equal(want) {
		t.Errorf("Date() = %v, want %v", it.Date(), want)
	}

	q := (*queries)[0]
	for k, want := range map[string]string{"page": "rss", "q": "big buck bunny", "c": "0_0", "f": "2", "u": "someone"} {
		if q[k] != want {
			t.Errorf("query %s = %q, want %q", k, q[k], want)
		}
	}
}

func TestSearchNyaaDefaults(t *testing.T) {
	queries := fakeNyaa(t, func(w http.ResponseWriter, r *http.Request) {
		fmt.Fprint(w, sampleFeed)
	})
	if _, err := searchNyaa(t.Context(), "", "", false); err != nil {
		t.Fatal(err)
	}
	q := (*queries)[0]
	if q["f"] != "0" {
		t.Errorf("filter = %q, want 0", q["f"])
	}
	if _, ok := q["u"]; ok {
		t.Error("user set without -user")
	}
}

func TestSearchNyaaErrors(t *testing.T) {
	status := http.StatusNotFound
	fakeNyaa(t, func(w http.ResponseWriter, r *http.Request) { w.WriteHeader(status) })

	if _, err := searchNyaa(t.Context(), "", "nobody", false); err == nil || !strings.Contains(err.Error(), `user "nobody" not found`) {
		t.Errorf("unknown user: err = %v", err)
	}
	status = http.StatusTooManyRequests
	if _, err := searchNyaa(t.Context(), "x", "", false); err == nil || !strings.Contains(err.Error(), "429") {
		t.Errorf("rate limited: err = %v", err)
	}
}

func TestInvalidItemDate(t *testing.T) {
	if d := (nyaaItem{PubDate: "yesterday"}).Date(); !d.IsZero() {
		t.Errorf("Date() = %v, want zero", d)
	}
}

func TestPrintResults(t *testing.T) {
	fakeNyaa(t, func(w http.ResponseWriter, r *http.Request) { fmt.Fprint(w, sampleFeed) })
	var buf bytes.Buffer
	printResults(t.Context(), &buf, "bunny", "", false)
	lines := strings.Split(strings.TrimSuffix(buf.String(), "\n"), "\n")
	if len(lines) != 2 {
		t.Fatalf("got %d lines, want 2:\n%s", len(lines), buf.String())
	}
	fields := strings.Split(lines[0], "\t")
	if len(fields) != 3 {
		t.Fatalf("got %d tab separated fields, want 3: %q", len(fields), lines[0])
	}
	if fields[0] != "https://nyaa.si/download/1.torrent" {
		t.Errorf("URL field = %q", fields[0])
	}
	if !strings.Contains(fields[1], "2026-09-26") || !strings.Contains(fields[1], "1.2 GiB") {
		t.Errorf("columns field = %q", fields[1])
	}
	if fields[2] != "[Group] Big Buck Bunny - 01 [1080p].mkv" {
		t.Errorf("title field = %q", fields[2])
	}
}

func TestPrintResultsFailedSearch(t *testing.T) {
	fakeNyaa(t, func(w http.ResponseWriter, r *http.Request) { w.WriteHeader(http.StatusServiceUnavailable) })
	var buf bytes.Buffer
	printResults(t.Context(), &buf, "bunny", "", false)
	if buf.Len() != 0 {
		t.Errorf("failed search printed %q", buf.String())
	}
}

func TestHealth(t *testing.T) {
	tests := []struct {
		seeders, leechers int
		color             string
	}{
		{0, 5, "\033[31m"},
		{0, 0, "\033[31m"},
		{10, 2, "\033[32m"},
		{3, 3, "\033[33m"},
		{1, 9, "\033[38;5;208m"},
	}
	for _, tt := range tests {
		got := health(nyaaItem{Seeders: tt.seeders, Leechers: tt.leechers})
		if !strings.HasPrefix(got, tt.color) {
			t.Errorf("health(%d seeders, %d leechers) = %q, want color %q", tt.seeders, tt.leechers, got, tt.color)
		}
	}
}

func TestShellQuote(t *testing.T) {
	tests := map[string]string{
		"":                 "''",
		"plain":            "'plain'",
		"/path with space": "'/path with space'",
		"it's":             `'it'\''s'`,
	}
	for in, want := range tests {
		if got := shellQuote(in); got != want {
			t.Errorf("shellQuote(%q) = %q, want %q", in, got, want)
		}
	}
}
