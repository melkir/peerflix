//! The tables the window lists search results and a torrent's episodes in.
//! Their first column takes the width the others leave.

use std::{cmp::Ordering, collections::HashMap, path::Path};

use gpui_kit::component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, h_flex,
    table::{Column, TableDelegate, TableState},
};
use gpui_kit::{
    App, Context, InteractiveElement as _, IntoElement, ParentElement as _, Pixels, SharedString,
    StatefulInteractiveElement as _, Styled as _, Window, div, px,
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

/// A search result as the table shows it, made once when it arrives. Its
/// text is shared, so copies of cached results are cheap.
#[derive(Clone)]
pub struct Found {
    /// The magnet or .torrent URL.
    pub url: SharedString,
    pub title: SharedString,
    size: SharedString,
    seeders: SharedString,
    /// Seeders compared to leechers.
    health: Ordering,
    date: SharedString,
    site: &'static str,
    /// Where it came among the results, for undoing a sort.
    order: usize,
    /// The size in bytes, and seeders, to sort by.
    bytes: f64,
    seeder_count: u32,
}

impl Found {
    pub fn new(site: &'static str, t: Torrent) -> Self {
        Found {
            seeders: t.seeders.to_string().into(),
            health: t.seeders.cmp(&t.leechers),
            bytes: size_bytes(&t.size),
            seeder_count: t.seeders,
            url: t.url.into(),
            title: t.title.into(),
            size: t.size.into(),
            date: t.date.into(),
            site,
            order: 0,
        }
    }
}

/// The bytes a size such as 1.4 GiB stands for, or 0 if it isn't one.
fn size_bytes(size: &str) -> f64 {
    const UNITS: [&str; 7] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB", "EiB"];
    let Some((n, unit)) = size.trim().split_once(' ') else {
        return 0.;
    };
    let (Ok(n), Some(exp)) = (n.parse::<f64>(), UNITS.iter().position(|&u| u == unit)) else {
        return 0.;
    };
    n * 1024f64.powi(exp as i32)
}

/// The search results, in the order the sites answered, each site's in its
/// own order, unless sorted by a column.
pub struct Results {
    found: Vec<Found>,
    /// Whether a search is running, which shows while there's nothing yet.
    pub searching: bool,
    sort: Option<Sort>,
    /// How many sites were searched; with one, the site column is left out.
    sites: usize,
    title_width: Pixels,
}

/// The results' columns, in order. The site's is left out with one site.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Col {
    Title,
    Size,
    Seeders,
    Date,
    Site,
}

impl Col {
    /// The column at index ix of the table.
    fn at(ix: usize) -> Col {
        match ix {
            0 => Col::Title,
            1 => Col::Size,
            2 => Col::Seeders,
            3 => Col::Date,
            _ => Col::Site,
        }
    }

    /// Whether the column sorts in descending order first: the numbers and
    /// dates do, biggest and newest first, and the text doesn't.
    fn descending_first(self) -> bool {
        matches!(self, Col::Size | Col::Seeders | Col::Date)
    }

    /// How a and b compare in the column.
    fn compare(self, a: &Found, b: &Found) -> Ordering {
        match self {
            // Ignoring case, without lowercasing a copy of each title for
            // every comparison.
            Col::Title => a
                .title
                .chars()
                .flat_map(char::to_lowercase)
                .cmp(b.title.chars().flat_map(char::to_lowercase)),
            Col::Size => a.bytes.total_cmp(&b.bytes),
            Col::Seeders => a.seeder_count.cmp(&b.seeder_count),
            Col::Date => a.date.cmp(&b.date),
            Col::Site => a.site.cmp(b.site),
        }
    }
}

/// The column the results are sorted by.
#[derive(Clone, Copy, Debug)]
struct Sort {
    col: Col,
    descending: bool,
}

// Wide enough for the name and the sort arrow.
const SIZE: Pixels = px(90.);
const SEEDERS: Pixels = px(96.);
const DATE: Pixels = px(100.);
const SITE: Pixels = px(64.);

impl Results {
    pub fn new() -> Self {
        Results {
            found: Vec::new(),
            searching: false,
            sort: None,
            sites: 1,
            title_width: MIN_WIDTH,
        }
    }

    /// The results, in the order they're shown.
    pub fn found(&self) -> &[Found] {
        &self.found
    }

    /// The results in the order they came, whatever the sort.
    pub fn in_order(&self) -> Vec<Found> {
        let mut found = self.found.clone();
        found.sort_by_key(|f| f.order);
        found
    }

    pub fn clear(&mut self) {
        self.found.clear();
    }

    /// Replaces the results with found, which is in the order it came.
    pub fn set(&mut self, found: Vec<Found>) {
        self.found = found;
        for (order, f) in self.found.iter_mut().enumerate() {
            f.order = order;
        }
        self.apply_sort();
    }

    /// Adds a site's results after the others, sorted among them if they're
    /// sorted.
    pub fn add(&mut self, site: &'static str, torrents: Vec<Torrent>) {
        let start = self.found.len();
        self.found
            .extend(torrents.into_iter().enumerate().map(|(i, t)| Found {
                order: start + i,
                ..Found::new(site, t)
            }));
        self.apply_sort();
    }

    /// Sorts by column col: in its first direction, then the other, then
    /// back to the order the results came in.
    fn sort_by(&mut self, col: Col) {
        let first = col.descending_first();
        self.sort = match self.sort {
            Some(s) if s.col == col && s.descending == first => Some(Sort {
                col,
                descending: !first,
            }),
            Some(s) if s.col == col => None,
            _ => Some(Sort {
                col,
                descending: first,
            }),
        };
        self.apply_sort();
    }

    fn apply_sort(&mut self) {
        let Some(Sort { col, descending }) = self.sort else {
            self.found.sort_by_key(|f| f.order);
            return;
        };
        self.found.sort_by(|a, b| {
            let ord = col.compare(a, b);
            let ord = if descending { ord.reverse() } else { ord };
            ord.then(a.order.cmp(&b.order))
        });
    }

    /// Changes the results with change, keeping the row selected in table
    /// selected wherever it moves.
    pub fn keeping_selection(
        table: &mut TableState<Self>,
        cx: &mut Context<TableState<Self>>,
        change: impl FnOnce(&mut Self),
    ) {
        let order =
            |table: &TableState<Self>, row: usize| table.delegate().found.get(row).map(|f| f.order);
        let selected = table.selected_row();
        let was = selected.and_then(|row| order(table, row));
        change(table.delegate_mut());
        let now = was.and_then(|o| table.delegate().found.iter().position(|f| f.order == o));
        if let Some(row) = now.filter(|&row| Some(row) != selected) {
            table.set_selected_row(row, cx);
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
        match Col::at(col_ix) {
            Col::Title => Column::new("title", "Title").width(self.title_width),
            Col::Size => Column::new("size", "Size").width(SIZE).text_right(),
            Col::Seeders => Column::new("seeders", "Seeders")
                .width(SEEDERS)
                .text_right(),
            Col::Date => Column::new("date", "Date").width(DATE),
            Col::Site => Column::new("site", "Site").width(SITE),
        }
    }

    /// The column's name, with an arrow when the results are sorted by it,
    /// which a click sorts them by, showing the first from the top.
    fn render_th(
        &mut self,
        col_ix: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let col = Col::at(col_ix);
        let arrow = self
            .sort
            .filter(|s| s.col == col)
            .map(|s| match s.descending {
                true => IconName::SortDescending,
                false => IconName::SortAscending,
            });
        h_flex()
            .id(("sort", col_ix))
            .size_full()
            .gap_1()
            .cursor_pointer()
            .child(self.column(col_ix, cx).name)
            .children(arrow.map(|icon| Icon::new(icon).small()))
            .on_click(cx.listener(move |table, _, _, cx| {
                table.delegate_mut().sort_by(col);
                if !table.delegate().found.is_empty() {
                    table.set_selected_row(0, cx);
                }
                cx.notify();
            }))
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
        match Col::at(col_ix) {
            Col::Title => div().truncate().child(found.title.clone()),
            Col::Size => div().text_color(muted).child(found.size.clone()),
            Col::Seeders => {
                // Healthy when seeders outnumber leechers, as on the command
                // line.
                let color = match found.health {
                    Ordering::Greater => cx.theme().green,
                    Ordering::Equal => cx.theme().yellow,
                    Ordering::Less => cx.theme().red,
                };
                div().text_color(color).child(found.seeders.clone())
            }
            Col::Date => div().text_color(muted).child(found.date.clone()),
            Col::Site => div().text_color(muted).child(found.site),
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

    fn torrent(title: &str, size: &str, seeders: u32) -> Torrent {
        Torrent {
            title: title.into(),
            size: size.into(),
            seeders,
            ..Torrent::default()
        }
    }

    fn titles(results: &Results) -> Vec<&str> {
        results.found().iter().map(|f| f.title.as_ref()).collect()
    }

    #[test]
    fn sorts_by_a_column_then_back() {
        let mut results = Results::new();
        results.add(
            "yts",
            vec![
                torrent("b", "700.0 MiB", 5),
                torrent("C", "1.4 GiB", 50),
                torrent("a", "2.0 GiB", 0),
            ],
        );
        // Seeders sort most first, then fewest, then as they came.
        results.sort_by(Col::Seeders);
        assert_eq!(titles(&results), ["C", "b", "a"]);
        results.sort_by(Col::Seeders);
        assert_eq!(titles(&results), ["a", "b", "C"]);
        results.sort_by(Col::Seeders);
        assert_eq!(titles(&results), ["b", "C", "a"]);
        // Titles sort A to Z first, ignoring case.
        results.sort_by(Col::Title);
        assert_eq!(titles(&results), ["a", "b", "C"]);
        // Sizes sort by bytes, not by their text.
        results.sort_by(Col::Size);
        assert_eq!(titles(&results), ["a", "C", "b"]);
    }

    #[test]
    fn sorts_results_as_they_arrive() {
        let mut results = Results::new();
        results.sort_by(Col::Seeders);
        results.add("yts", vec![torrent("few", "1 B", 1)]);
        results.add("tpb", vec![torrent("many", "1 B", 9)]);
        assert_eq!(titles(&results), ["many", "few"]);
        // Back to the order they came.
        results.sort_by(Col::Seeders);
        results.sort_by(Col::Seeders);
        assert_eq!(titles(&results), ["few", "many"]);
        assert_eq!(results.in_order()[0].title.as_ref(), "few");
    }

    #[test]
    fn reads_sizes() {
        for (size, want) in [
            ("1023 B", 1023.),
            ("1.5 KiB", 1536.),
            ("2.0 GiB", 2. * f64::from(1 << 30)),
            ("", 0.),
            ("big", 0.),
            ("3 parsecs", 0.),
        ] {
            assert_eq!(size_bytes(size), want, "{size:?}");
        }
    }

    #[test]
    fn first_column_takes_the_rest() {
        let mut results = Results::new();
        assert!(results.fit(px(1000.), 2));
        assert_eq!(results.title_width, px(1000.) - px(350.) - CHROME);
        // One site leaves out the site column, giving its width to the title.
        assert!(results.fit(px(1000.), 1));
        assert_eq!(results.title_width, px(1000.) - px(286.) - CHROME);
        assert!(!results.fit(px(1000.), 1));
        // It never gets narrower than MIN_WIDTH.
        results.fit(px(300.), 1);
        assert_eq!(results.title_width, MIN_WIDTH);
    }
}
