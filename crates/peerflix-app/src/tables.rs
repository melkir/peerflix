//! The tables the window lists search results and a torrent's episodes in.
//! Their first column takes the width the others leave.

use std::{cmp::Ordering, collections::HashMap, path::Path};

use gpui_kit::component::{
    ActiveTheme as _,
    table::{Column, TableDelegate, TableState},
};
use gpui_kit::{
    App, Context, IntoElement, ParentElement as _, Pixels, SharedString, Styled as _, Window, div,
    px,
};
use peerflix_core::{providers::Torrent, torrent::files::TorrentFile, util::human_bytes};

/// Width the table takes besides its columns: its border and the filler
/// column after the last.
const CHROME: Pixels = px(18.);

/// The narrowest the first column gets.
const MIN_WIDTH: Pixels = px(200.);

/// The width left for the first column in a table width wide, besides the
/// others, rest wide.
fn remaining(width: Pixels, rest: Pixels) -> Pixels {
    (width - rest - CHROME).max(MIN_WIDTH)
}

/// A search result as the table shows it, made once when it arrives.
pub struct Found {
    /// The magnet or .torrent URL.
    pub url: String,
    pub title: SharedString,
    size: SharedString,
    seeders: SharedString,
    /// Seeders compared to leechers.
    health: Ordering,
    date: SharedString,
    site: &'static str,
}

impl Found {
    pub fn new(site: &'static str, t: Torrent) -> Self {
        Found {
            seeders: t.seeders.to_string().into(),
            health: t.seeders.cmp(&t.leechers),
            url: t.url,
            title: t.title.into(),
            size: t.size.into(),
            date: t.date.into(),
            site,
        }
    }
}

/// The search results, in the order the sites answered, each site's in its
/// own order.
pub struct Results {
    pub found: Vec<Found>,
    /// Whether a search is running, which shows while there's nothing yet.
    pub searching: bool,
    /// How many sites were searched; with one, the site column is left out.
    sites: usize,
    title_width: Pixels,
}

const SIZE: Pixels = px(90.);
const SEEDERS: Pixels = px(80.);
const DATE: Pixels = px(100.);
const SITE: Pixels = px(60.);

impl Results {
    pub fn new() -> Self {
        Results {
            found: Vec::new(),
            searching: false,
            sites: 1,
            title_width: MIN_WIDTH,
        }
    }

    /// Fits the columns to a table width wide, for sites sites. Returns
    /// whether they changed, which needs the table refreshed.
    pub fn fit(&mut self, width: Pixels, sites: usize) -> bool {
        let site = if sites > 1 { SITE } else { px(0.) };
        let title_width = remaining(width, SIZE + SEEDERS + DATE + site);
        let changed = (title_width, sites) != (self.title_width, self.sites);
        (self.title_width, self.sites) = (title_width, sites);
        changed
    }
}

impl TableDelegate for Results {
    fn columns_count(&self, _: &App) -> usize {
        if self.sites > 1 { 5 } else { 4 }
    }

    fn rows_count(&self, _: &App) -> usize {
        self.found.len()
    }

    fn column(&self, col_ix: usize, _: &App) -> Column {
        match col_ix {
            0 => Column::new("title", "Title").width(self.title_width),
            1 => Column::new("size", "Size").width(SIZE).text_right(),
            2 => Column::new("seeders", "Seeders")
                .width(SEEDERS)
                .text_right(),
            3 => Column::new("date", "Date").width(DATE),
            _ => Column::new("site", "Site").width(SITE),
        }
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let found = &self.found[row_ix];
        let muted = cx.theme().muted_foreground;
        match col_ix {
            0 => div().truncate().child(found.title.clone()),
            1 => div().text_color(muted).child(found.size.clone()),
            2 => {
                // Healthy when seeders outnumber leechers, as on the command
                // line.
                let color = match found.health {
                    Ordering::Greater => cx.theme().green,
                    Ordering::Equal => cx.theme().yellow,
                    Ordering::Less => cx.theme().red,
                };
                div().text_color(color).child(found.seeders.clone())
            }
            3 => div().text_color(muted).child(found.date.clone()),
            _ => div().text_color(muted).child(found.site),
        }
    }

    fn loading(&self, _: &App) -> bool {
        self.searching && self.found.is_empty()
    }
}

/// A torrent's episodes to choose between.
pub struct Episodes {
    /// The file ids of the episodes, in order, with the names to show and
    /// sizes.
    pub files: Vec<(usize, SharedString, SharedString)>,
    name_width: Pixels,
}

const EPISODE_SIZE: Pixels = px(100.);

impl Episodes {
    /// The episodes, the ids eps, of the torrent's files, each shown by its
    /// file name, or its path when another has the same name.
    pub fn new(files: &[TorrentFile], eps: &[usize], width: Pixels) -> Self {
        let name = |i: usize| {
            let path = &files[i].path;
            Path::new(path)
                .file_name()
                .map_or(path.as_str(), |n| n.to_str().unwrap_or(path))
        };
        let mut counts = HashMap::new();
        for &i in eps {
            *counts.entry(name(i)).or_insert(0) += 1;
        }
        let files = eps
            .iter()
            .map(|&i| {
                let shown = if counts[name(i)] > 1 {
                    files[i].path.as_str()
                } else {
                    name(i)
                };
                (i, shown.to_owned().into(), human_bytes(files[i].len).into())
            })
            .collect();
        let mut episodes = Episodes {
            files,
            name_width: MIN_WIDTH,
        };
        episodes.fit(width);
        episodes
    }

    /// Fits the columns to a table width wide. Returns whether they changed,
    /// which needs the table refreshed.
    pub fn fit(&mut self, width: Pixels) -> bool {
        let name_width = remaining(width, EPISODE_SIZE);
        let changed = name_width != self.name_width;
        self.name_width = name_width;
        changed
    }
}

impl TableDelegate for Episodes {
    fn columns_count(&self, _: &App) -> usize {
        2
    }

    fn rows_count(&self, _: &App) -> usize {
        self.files.len()
    }

    fn column(&self, col_ix: usize, _: &App) -> Column {
        match col_ix {
            0 => Column::new("name", "Episode").width(self.name_width),
            _ => Column::new("size", "Size").width(EPISODE_SIZE).text_right(),
        }
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let (_, name, size) = &self.files[row_ix];
        match col_ix {
            0 => div().truncate().child(name.clone()),
            _ => div()
                .text_color(cx.theme().muted_foreground)
                .child(size.clone()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn episode_names() {
        let file = |path: &str| TorrentFile {
            path: path.into(),
            len: 1 << 30,
            padding: false,
        };
        let files = [
            file("Pioneer.One.S01/Pioneer.One.S01E01.mkv"),
            file("Pioneer.One.S01/Pioneer.One.S01E02.mkv"),
            file("Season 1/01.mkv"),
            file("Season 2/01.mkv"),
        ];
        let eps = Episodes::new(&files, &[0, 1, 2, 3], px(800.));
        let names: Vec<_> = eps.files.iter().map(|(_, n, _)| n.as_ref()).collect();
        assert_eq!(
            names,
            [
                "Pioneer.One.S01E01.mkv",
                "Pioneer.One.S01E02.mkv",
                // The same name twice: the paths tell them apart.
                "Season 1/01.mkv",
                "Season 2/01.mkv",
            ]
        );
    }

    #[test]
    fn first_column_takes_the_rest() {
        let mut results = Results::new();
        assert!(results.fit(px(1000.), 2));
        assert_eq!(results.title_width, px(1000.) - px(330.) - CHROME);
        // One site leaves out the site column, giving its width to the title.
        assert!(results.fit(px(1000.), 1));
        assert_eq!(results.title_width, px(1000.) - px(270.) - CHROME);
        assert!(!results.fit(px(1000.), 1));
        // It never gets narrower than MIN_WIDTH.
        results.fit(px(300.), 1);
        assert_eq!(results.title_width, MIN_WIDTH);
    }
}
