use anyhow::{Context, bail};
use serde::Deserialize;

pub const NYAA_URL: &str = "https://nyaa.si";

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Item {
    pub title: String,
    #[serde(rename = "link")]
    pub torrent: String,
    #[serde(rename = "guid")]
    pub view: String,
    #[serde(rename = "pubDate")]
    pub pub_date: String,
    // The nyaa: namespace elements; quick-xml matches them by local name.
    pub seeders: u32,
    pub leechers: u32,
    #[serde(rename = "infoHash")]
    pub info_hash: String,
    pub category: String,
    pub size: String,
}

impl Item {
    /// The publication date as YYYY-MM-DD in the feed's time zone, or
    /// 0001-01-01 if pubDate isn't an RFC 1123 date with a numeric zone.
    pub fn date(&self) -> String {
        parse_rfc1123z(&self.pub_date).unwrap_or_else(|| "0001-01-01".to_owned())
    }
}

fn parse_rfc1123z(s: &str) -> Option<String> {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let [weekday, day, month, year, time, zone] =
        s.split(' ').collect::<Vec<_>>().try_into().ok()?;
    let digits = |s: &str, n: usize| s.len() == n && s.bytes().all(|b| b.is_ascii_digit());
    let valid = weekday.len() == 4
        && weekday.ends_with(',')
        && digits(day, 2)
        && (1..=31).contains(&day.parse::<u8>().ok()?)
        && digits(year, 4)
        && time.len() == 8
        && time.split(':').all(|p| digits(p, 2))
        && zone.len() == 5
        && zone.starts_with(['+', '-'])
        && digits(&zone[1..], 4);
    let month = MONTHS.iter().position(|&m| m == month)? + 1;
    valid.then(|| format!("{year}-{month:02}-{day}"))
}

/// Queries nyaa's RSS feed across all categories, which returns up to 75
/// results sorted newest first. A non-empty user restricts results to that
/// uploader; trusted excludes uploads from untrusted users.
pub async fn search(
    base: &str,
    query: &str,
    user: &str,
    trusted: bool,
) -> anyhow::Result<Vec<Item>> {
    let filter = if trusted { "2" } else { "0" };
    let mut params = vec![("page", "rss"), ("q", query), ("c", "0_0"), ("f", filter)];
    if !user.is_empty() {
        params.push(("u", user));
    }
    let resp = reqwest::Client::new()
        .get(format!("{base}/"))
        .query(&params)
        .send()
        .await
        .context("searching nyaa")?;
    let status = resp.status();
    if status == reqwest::StatusCode::NOT_FOUND && !user.is_empty() {
        bail!("nyaa user {user:?} not found");
    }
    if status != reqwest::StatusCode::OK {
        bail!("searching nyaa: {status}");
    }
    let body = resp.text().await.context("searching nyaa")?;

    #[derive(Deserialize)]
    struct Rss {
        channel: Channel,
    }
    #[derive(Deserialize)]
    struct Channel {
        #[serde(default)]
        item: Vec<Item>,
    }
    let rss: Rss = quick_xml::de::from_str(&body).context("parsing nyaa results")?;
    Ok(rss.channel.item)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::testutil::FakeServer;

    pub const SAMPLE_FEED: &str = r#"<?xml version="1.0" encoding="utf-8"?>
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
</rss>"#;

    #[tokio::test]
    async fn search_parses_feed() {
        let srv = FakeServer::start(200, SAMPLE_FEED).await;
        let items = search(&srv.url, "big buck bunny", "someone", true)
            .await
            .unwrap();
        assert_eq!(items.len(), 2);
        let it = &items[0];
        assert_eq!(it.title, "[Group] Big Buck Bunny - 01 [1080p].mkv");
        assert_eq!(it.torrent, "https://nyaa.si/download/1.torrent");
        assert_eq!(it.view, "https://nyaa.si/view/1");
        assert_eq!((it.seeders, it.leechers), (42, 3));
        assert_eq!(it.info_hash, "0123456789abcdef0123456789abcdef01234567");
        assert_eq!(it.category, "Anime - English-translated");
        assert_eq!(it.size, "1.2 GiB");
        assert_eq!(it.date(), "2026-09-26");
        assert_eq!(items[1].info_hash, "");

        let q = srv.queries().remove(0);
        for (k, want) in [
            ("page", "rss"),
            ("q", "big buck bunny"),
            ("c", "0_0"),
            ("f", "2"),
            ("u", "someone"),
        ] {
            assert_eq!(q.get(k).map(String::as_str), Some(want), "query {k}");
        }
    }

    #[tokio::test]
    async fn search_defaults() {
        let srv = FakeServer::start(200, SAMPLE_FEED).await;
        search(&srv.url, "", "", false).await.unwrap();
        let q = srv.queries().remove(0);
        assert_eq!(q.get("f").map(String::as_str), Some("0"));
        assert!(!q.contains_key("u"), "user set without --user");
    }

    #[tokio::test]
    async fn search_errors() {
        let srv = FakeServer::start(404, "").await;
        let err = search(&srv.url, "", "nobody", false).await.unwrap_err();
        assert!(
            err.to_string().contains(r#"user "nobody" not found"#),
            "{err}"
        );

        let srv = FakeServer::start(429, "").await;
        let err = search(&srv.url, "x", "", false).await.unwrap_err();
        assert!(err.to_string().contains("429"), "{err}");
    }

    #[test]
    fn invalid_dates() {
        for s in [
            "yesterday",
            "",
            "Sat, 26 Foo 2026 12:00:00 -0000",
            "Sat, 26 Sep 2026 12:00:00 GMT",
        ] {
            let it = Item {
                pub_date: s.into(),
                ..Item::default()
            };
            assert_eq!(it.date(), "0001-01-01", "{s:?}");
        }
    }
}
