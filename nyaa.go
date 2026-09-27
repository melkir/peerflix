package main

import (
	"context"
	"encoding/xml"
	"fmt"
	"net/http"
	"net/url"
	"time"
)

// nyaaURL is a variable so tests can point it at a local server.
var nyaaURL = "https://nyaa.si"

type nyaaItem struct {
	Title    string `xml:"title"`
	Torrent  string `xml:"link"`
	View     string `xml:"guid"`
	PubDate  string `xml:"pubDate"`
	Seeders  int    `xml:"https://nyaa.si/xmlns/nyaa seeders"`
	Leechers int    `xml:"https://nyaa.si/xmlns/nyaa leechers"`
	InfoHash string `xml:"https://nyaa.si/xmlns/nyaa infoHash"`
	Category string `xml:"https://nyaa.si/xmlns/nyaa category"`
	Size     string `xml:"https://nyaa.si/xmlns/nyaa size"`
}

func (it nyaaItem) Date() time.Time {
	t, _ := time.Parse(time.RFC1123Z, it.PubDate)
	return t
}

// searchNyaa queries nyaa's RSS feed across all categories, which returns up
// to 75 results sorted newest first. A non-empty user restricts results to
// that uploader; trusted excludes uploads from untrusted users.
func searchNyaa(ctx context.Context, query, user string, trusted bool) ([]nyaaItem, error) {
	filter := "0"
	if trusted {
		filter = "2"
	}
	q := url.Values{"page": {"rss"}, "q": {query}, "c": {"0_0"}, "f": {filter}}
	if user != "" {
		q.Set("u", user)
	}
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, nyaaURL+"/?"+q.Encode(), nil)
	if err != nil {
		return nil, err
	}
	resp, err := http.DefaultClient.Do(req)
	if err != nil {
		return nil, fmt.Errorf("searching nyaa: %w", err)
	}
	defer resp.Body.Close()
	if resp.StatusCode == http.StatusNotFound && user != "" {
		return nil, fmt.Errorf("nyaa user %q not found", user)
	}
	if resp.StatusCode != http.StatusOK {
		return nil, fmt.Errorf("searching nyaa: %s", resp.Status)
	}
	var feed struct {
		Items []nyaaItem `xml:"channel>item"`
	}
	if err := xml.NewDecoder(resp.Body).Decode(&feed); err != nil {
		return nil, fmt.Errorf("parsing nyaa results: %w", err)
	}
	return feed.Items, nil
}
