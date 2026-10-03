//! The window: a search over a category's sites, a torrent's episodes to pick
//! from, and the streams playing.

use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

use gpui_kit::component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, Theme, TitleBar, WindowExt as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputEvent, InputState},
    notification::Notification,
    spinner::Spinner,
    tab::{Tab, TabBar},
    table::{DataTable, TableDelegate, TableEvent, TableState},
    v_flex,
};
use gpui_kit::{
    App, AppContext as _, Context, Entity, FocusHandle, Focusable as _, InteractiveElement as _,
    IntoElement, MouseButton, ParentElement as _, Pixels, Point, Render, SharedString, Styled as _,
    Subscription, Task, Window, div, point, prelude::FluentBuilder as _, px,
};
// The full Lucide catalog's names, beside gpui-component's IconName.
use gpui_kit::assets::IconName as AssetIcon;
use peerflix_core::{
    play::{self, Listing},
    providers::{Provider, Query, Torrent},
    search::{self, Category, Endpoints, Failed},
    torrent::files::pick_file,
};
use tokio::sync::mpsc;

use crate::{
    Back, Confirm, NextCategory, PrevCategory, SelectNext, SelectPrev,
    runtime::{self, Runtime},
    streams::Streams,
    tables::{Episodes, Found, Results},
};

/// How long typing pauses before the sites are searched.
const DEBOUNCE: Duration = Duration::from_millis(250);

/// How long fetching a torrent's files goes before the window says it can
/// take a while.
const SLOW: Duration = Duration::from_secs(15);

/// How long a category's results without search terms are shown again
/// without searching anew. Older ones still show at once while they refresh.
const FRESH: Duration = Duration::from_secs(5 * 60);

/// The title bar's height, a toolbar's, as Transmission's.
const TITLE_BAR_HEIGHT: f32 = 52.;

/// The traffic lights' height, to center them in the title bar.
const TRAFFIC_LIGHT_SIZE: f32 = 14.;

/// Where the traffic lights go, centered in the title bar.
pub const TRAFFIC_LIGHTS: Point<Pixels> =
    point(px(18.), px((TITLE_BAR_HEIGHT - TRAFFIC_LIGHT_SIZE) / 2.));

/// How far the title bar's controls stay from its left edge, clear of the
/// traffic lights.
const TRAFFIC_LIGHTS_WIDTH: Pixels = px(88.);

pub struct Peerflix {
    category: Category,
    endpoints: Endpoints,
    /// The category's sites.
    providers: Vec<Arc<dyn Provider>>,
    query: Entity<InputState>,
    results: Entity<TableState<Results>>,
    /// Why the last search found nothing, shown under nothing_icon in place
    /// of the table. Problems beside results are notifications instead.
    note: SharedString,
    /// Pictures why the last search found nothing.
    nothing_icon: AssetIcon,
    /// The search running, if any; dropping it stops it.
    search: Option<Task<()>>,
    /// Each category's results without search terms, by its index, to show
    /// at once when its tab is picked again or the search box is cleared.
    browse: [Option<Browse>; Category::ALL.len()],
    page: Page,
    /// The torrents opened so far, by URL, to show their episodes again at
    /// once rather than fetching their files anew.
    listings: HashMap<SharedString, Arc<Listing>>,
    streams: Entity<Streams>,
    /// The window's width, which the tables' first columns fill.
    width: Pixels,
    _subscriptions: Vec<Subscription>,
}

/// Identifies the notification that some sites' search failed, so a new one
/// replaces it rather than piling up as each keystroke searches.
struct SitesFailed;

/// A category's results without search terms, as they were last found.
struct Browse {
    found: Vec<Found>,
    note: SharedString,
    at: Instant,
}

/// What the window shows below the search box.
enum Page {
    Results,
    /// Fetching the files of the torrent titled title, slow once it has
    /// taken SLOW; dropping the tasks stops it.
    Opening {
        title: SharedString,
        slow: bool,
        _listing: Task<()>,
        _slow: Task<()>,
    },
    /// The episodes of the torrent titled title, to pick one after another.
    Episodes {
        title: SharedString,
        listing: Arc<Listing>,
        table: Entity<TableState<Episodes>>,
        _subscription: Subscription,
    },
}

impl Peerflix {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        // Light or dark as the system is, following it when it changes.
        Theme::sync_system_appearance(Some(window), cx);
        let query = cx.new(|cx| InputState::new(window, cx).placeholder("Search anime"));
        // A header click sorts the results rather than selecting its column.
        let results =
            cx.new(|cx| TableState::new(Results::new(), window, cx).col_selectable(false));
        let subscriptions = vec![
            cx.subscribe_in(&query, window, |this, _, event, window, cx| match event {
                InputEvent::Change => {
                    // Typing searches anew, whose results should show.
                    if !matches!(this.page, Page::Results) {
                        this.back(window, cx);
                    }
                    this.search(true, window, cx);
                }
                InputEvent::PressEnter { .. } => this.confirm(window, cx),
                _ => {}
            }),
            cx.subscribe_in(&results, window, |this, _, event, window, cx| {
                if let TableEvent::DoubleClickedRow(_) = event {
                    this.open_selected(window, cx);
                }
            }),
            cx.observe_window_appearance(window, |_, window, cx| {
                Theme::sync_system_appearance(Some(window), cx);
            }),
            cx.observe_window_bounds(window, |this, window, cx| {
                this.width = window.viewport_size().width;
                this.fit_columns(cx);
            }),
        ];
        query.update(cx, |query, cx| query.focus(window, cx));
        let category = Category::Anime;
        let endpoints = Endpoints::from_env();
        let mut this = Peerflix {
            category,
            providers: category.providers(&endpoints, "", false),
            endpoints,
            query,
            results,
            note: SharedString::default(),
            nothing_icon: AssetIcon::SearchX,
            search: None,
            browse: Default::default(),
            page: Page::Results,
            listings: HashMap::new(),
            streams: cx.new(|_| Streams::default()),
            width: window.viewport_size().width,
            _subscriptions: subscriptions,
        };
        this.fit_columns(cx);
        // With no terms, that's the newest anime. The other tabs' are
        // fetched meanwhile, so they show at once.
        this.search(false, window, cx);
        for other in category.others() {
            this.prefetch(other, cx);
        }
        this
    }

    fn set_category(&mut self, category: Category, window: &mut Window, cx: &mut Context<Self>) {
        self.category = category;
        self.providers = self.category.providers(&self.endpoints, "", false);
        self.fit_columns(cx);
        let placeholder = format!("Search {}", self.category.name());
        self.query.update(cx, |query, cx| {
            query.set_placeholder(placeholder, window, cx);
            query.focus(window, cx);
        });
        self.back(window, cx);
        self.search(false, window, cx);
    }

    /// Searches the category's sites for what's typed, after DEBOUNCE if
    /// debounce, replacing the search running. The results so far stay until
    /// the first site answers. Without search terms, the category's cached
    /// results show at once instead, searched anew only once they're older
    /// than FRESH.
    fn search(&mut self, debounce: bool, window: &mut Window, cx: &mut Context<Self>) {
        let query = Query::new(&self.query.read(cx).value());
        if is_browse(&query) && self.show_browse(cx) {
            return;
        }
        // What the last search found, or didn't, no longer applies.
        self.note = SharedString::default();
        let providers = self.providers.clone();
        let client = cx.global::<Runtime>().client.clone();
        self.results.update(cx, |table, cx| {
            table.delegate_mut().searching = true;
            cx.notify();
        });
        let (found, mut answers) = mpsc::unbounded_channel();
        let failed = runtime::spawn(cx, {
            let query = query.clone();
            async move {
                if debounce {
                    tokio::time::sleep(DEBOUNCE).await;
                }
                search::search(&client, &providers, &query, |site, torrents| {
                    let _ = found.send((site, torrents));
                })
                .await
            }
        });
        self.search = Some(cx.spawn_in(window, async move |this, cx| {
            let mut answered = false;
            while let Some((site, torrents)) = answers.recv().await {
                let first = !std::mem::replace(&mut answered, true);
                let added = this.update(cx, |this, cx| this.add_results(site, torrents, first, cx));
                if added.is_err() {
                    return;
                }
            }
            let failed = failed.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.end_search(&query, &failed, answered, window, cx);
            });
        }));
    }

    /// Shows the category's cached results without search terms, if there
    /// are any, stopping the search running. Returns whether they're fresh,
    /// needing no new search.
    fn show_browse(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(browse) = &self.browse[self.category.index()] else {
            return false;
        };
        self.search = None;
        self.note = browse.note.clone();
        // Only searches no site failed are cached.
        self.nothing_icon = AssetIcon::SearchX;
        let found = browse.found.clone();
        let fresh = browse.at.elapsed() < FRESH;
        self.results.update(cx, |table, cx| {
            let results = table.delegate_mut();
            results.set(found);
            results.searching = false;
            if !results.found().is_empty() {
                table.set_selected_row(0, cx);
            }
            cx.notify();
        });
        cx.notify();
        fresh
    }

    /// Searches category's sites without search terms in the background, to
    /// cache what they find.
    fn prefetch(&mut self, category: Category, cx: &mut Context<Self>) {
        let providers = category.providers(&self.endpoints, "", false);
        let client = cx.global::<Runtime>().client.clone();
        let sites = providers.len();
        let searched = runtime::spawn(cx, async move {
            let mut found = Vec::new();
            let failed = search::search(&client, &providers, &Query::new(""), |site, torrents| {
                found.extend(torrents.into_iter().map(|t| Found::new(site, t)));
            })
            .await;
            (found, failed)
        });
        cx.spawn(async move |this, cx| {
            let (found, failed) = searched.await;
            let _ = this.update(cx, |this, _| {
                this.cache_browse(category, found, &failed, sites);
            });
        })
        .detach();
    }

    /// Caches found as category's results without search terms, unless one
    /// of its sites, sites in all, failed, since a later search may not.
    fn cache_browse(
        &mut self,
        category: Category,
        found: Vec<Found>,
        failed: &Failed,
        sites: usize,
    ) {
        if failed.count() > 0 {
            return;
        }
        let note = search::summary(category, "", found.len(), failed, sites).into();
        self.browse[category.index()] = Some(Browse {
            found,
            note,
            at: Instant::now(),
        });
    }

    /// Adds a site's results, in place of the last search's for the first
    /// site to answer.
    fn add_results(
        &mut self,
        site: &'static str,
        torrents: Vec<Torrent>,
        first: bool,
        cx: &mut Context<Self>,
    ) {
        self.results.update(cx, |table, cx| {
            if first {
                let results = table.delegate_mut();
                results.clear();
                results.add(site, torrents);
                if !results.found().is_empty() {
                    table.set_selected_row(0, cx);
                }
            } else {
                Results::keeping_selection(table, cx, |results| results.add(site, torrents));
            }
            cx.notify();
        });
    }

    /// Ends the search for query, with failed holding the sites whose search
    /// failed, and none having answered with results unless answered. Sites
    /// that failed beside results are told in a notification.
    fn end_search(
        &mut self,
        query: &Query,
        failed: &Failed,
        answered: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let found = self.results.update(cx, |table, cx| {
            let results = table.delegate_mut();
            results.searching = false;
            if !answered {
                results.clear();
            }
            cx.notify();
            results.found().len()
        });
        let sites = self.providers.len();
        let summary = search::summary(self.category, &query.text, found, failed, sites);
        self.nothing_icon = nothing_icon(failed, sites);
        if found == 0 {
            self.note = summary.into();
        } else if !summary.is_empty() {
            window.push_notification(Notification::warning(summary).id::<SitesFailed>(), cx);
        }
        if is_browse(query) {
            let found = self.results.read(cx).delegate().in_order();
            self.cache_browse(self.category, found, failed, sites);
        }
        cx.notify();
    }

    /// Fits the tables' columns to the window's width.
    fn fit_columns(&mut self, cx: &mut Context<Self>) {
        let (width, sites) = (self.width, self.providers.len());
        self.results.update(cx, |table, cx| {
            if table.delegate_mut().fit(width, sites) {
                table.refresh(cx);
                cx.notify();
            }
        });
        if let Page::Episodes { table, .. } = &self.page {
            table.update(cx, |table, cx| {
                if table.delegate_mut().fit(width) {
                    table.refresh(cx);
                    cx.notify();
                }
            });
        }
    }

    /// Moves the selection in the table showing, the results or the
    /// episodes, by delta rows.
    fn select(&mut self, delta: isize, cx: &mut Context<Self>) {
        match &self.page {
            Page::Results => move_selection(&self.results, delta, cx),
            Page::Episodes { table, .. } => move_selection(table, delta, cx),
            Page::Opening { .. } => {}
        }
    }

    /// Plays the selected episode, or opens the selected result, whichever
    /// shows.
    fn confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.page {
            Page::Results => self.open_selected(window, cx),
            Page::Episodes { .. } => self.play_selected(cx),
            Page::Opening { .. } => {}
        }
    }

    /// Opens the selected result.
    fn open_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let table = self.results.read(cx);
        let Some(found) = table
            .selected_row()
            .and_then(|row| table.delegate().found().get(row))
        else {
            return;
        };
        let (url, title) = (found.url.clone(), found.title.clone());
        if let Some(listing) = self.listings.get(&url) {
            return self.show_listing(title, listing.clone(), window, cx);
        }
        let session = cx.global::<Runtime>().session.clone();
        let fetch = runtime::spawn(cx, {
            let url = url.clone();
            async move { play::list(&session, &url).await }
        });
        let listing = cx.spawn_in(window, {
            let title = title.clone();
            async move |this, cx| {
                let listed = fetch.await;
                let _ = this.update_in(cx, |this, window, cx| {
                    this.listed(title, url, listed, window, cx);
                });
            }
        });
        let slow = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SLOW).await;
            let _ = this.update(cx, |this, cx| {
                if let Page::Opening { slow, .. } = &mut this.page {
                    *slow = true;
                    cx.notify();
                }
            });
        });
        self.page = Page::Opening {
            title,
            slow: false,
            _listing: listing,
            _slow: slow,
        };
        cx.notify();
    }

    /// Keeps listing, the files of the torrent at url titled title, and
    /// shows them, or why they couldn't be fetched.
    fn listed(
        &mut self,
        title: SharedString,
        url: SharedString,
        listing: anyhow::Result<Listing>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match listing {
            Ok(listing) => {
                let listing = Arc::new(listing);
                self.listings.insert(url, listing.clone());
                self.show_listing(title, listing, window, cx);
            }
            Err(e) => {
                couldnt_open(&e, window, cx);
                self.back(window, cx);
            }
        }
    }

    /// Picks the episode to stream from listing, the files of the torrent
    /// titled title, or streams its only video.
    fn show_listing(
        &mut self,
        title: SharedString,
        listing: Arc<Listing>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if listing.episodes.len() < 2 {
            match pick_file(&listing.files, None) {
                Ok(id) => self.play(title, listing, id, cx),
                Err(e) => couldnt_open(&e, window, cx),
            }
            return self.back(window, cx);
        }
        let episodes = Episodes::new(&listing.files, &listing.episodes, self.width);
        let table = cx.new(|cx| {
            let mut table = TableState::new(episodes, window, cx).col_selectable(false);
            table.set_selected_row(0, cx);
            table
        });
        let subscription = cx.subscribe(&table, |this, _, event, cx| {
            if let TableEvent::DoubleClickedRow(_) = event {
                this.play_selected(cx);
            }
        });
        table.read(cx).focus_handle(cx).focus(window, cx);
        self.page = Page::Episodes {
            title,
            listing,
            table,
            _subscription: subscription,
        };
        cx.notify();
    }

    /// Streams the selected episode, staying on the episodes to play another.
    fn play_selected(&mut self, cx: &mut Context<Self>) {
        let Page::Episodes {
            title,
            listing,
            table,
            ..
        } = &self.page
        else {
            return;
        };
        let table = table.read(cx);
        let Some(&(id, ..)) = table
            .selected_row()
            .and_then(|row| table.delegate().files.get(row))
        else {
            return;
        };
        let (title, listing) = (title.clone(), listing.clone());
        self.play(title, listing, id, cx);
    }

    /// Goes back to the results, giving up the torrent being opened or picked
    /// from.
    fn back(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.page = Page::Results;
        self.query.update(cx, |query, cx| query.focus(window, cx));
        cx.notify();
    }

    /// Streams file id of listing, the files of the torrent titled title.
    fn play(
        &mut self,
        title: SharedString,
        listing: Arc<Listing>,
        id: usize,
        cx: &mut Context<Self>,
    ) {
        self.streams
            .update(cx, |streams, cx| streams.play(title, listing, id, cx));
    }

    /// The title bar, a toolbar as Transmission's: the categories and the
    /// search box.
    fn render_title_bar(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let selected = self.category.index();
        let searching = self.results.read(cx).delegate().searching;
        let theme = cx.theme();
        TitleBar::new()
            .h(px(TITLE_BAR_HEIGHT))
            // Of a piece with the window below it.
            .bg(theme.background)
            .border_b_0()
            // Fullscreen hides the traffic lights.
            .pl(if window.is_fullscreen() {
                px(0.)
            } else {
                TRAFFIC_LIGHTS_WIDTH
            })
            .pr_3()
            .child(
                h_flex()
                    .flex_1()
                    .gap_3()
                    .child(
                        // The tabs don't keep their clicks to themselves, and a
                        // click on one that moves would drag the window.
                        div()
                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                            .child(
                                TabBar::new("categories")
                                    .segmented()
                                    .selected_index(selected)
                                    .on_click(cx.listener(|this, ix: &usize, window, cx| {
                                        this.set_category(Category::ALL[*ix], window, cx);
                                    }))
                                    .children(
                                        Category::ALL
                                            .map(|c| Tab::new().label(capitalized(c.name()))),
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .flex_1()
                            // A filled pill, as ChatGPT's composer.
                            .child(
                                Input::new(&self.query)
                                    .rounded_full()
                                    .bg(theme.secondary)
                                    .when(searching, |input| {
                                        input.suffix(loader().color(theme.muted_foreground))
                                    }),
                            ),
                    ),
            )
    }

    /// Whether the last search found nothing, which shows in place of the
    /// results.
    fn found_nothing(&self, cx: &App) -> bool {
        let results = self.results.read(cx).delegate();
        matches!(self.page, Page::Results) && results.found().is_empty() && !results.searching
    }

    fn render_page(&self, cx: &mut Context<Self>) -> impl IntoElement {
        match &self.page {
            Page::Results if self.found_nothing(cx) => v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .gap_4()
                .px_8()
                .child(
                    div()
                        .text_color(cx.theme().muted_foreground.opacity(0.6))
                        .child(Icon::new(self.nothing_icon).size_12()),
                )
                .child(
                    div()
                        .max_w(px(560.))
                        .text_center()
                        .text_color(cx.theme().muted_foreground)
                        .child(self.note.clone()),
                )
                .into_any_element(),
            Page::Results => div()
                .size_full()
                .child(framed(DataTable::new(&self.results), cx))
                .into_any_element(),
            Page::Opening { title, slow, .. } => v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .gap_3()
                .child(loader().large())
                .child("Fetching the torrent's files…")
                .child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child(title.clone()),
                )
                .when(*slow, |el| {
                    el.child(
                        div()
                            .max_w_96()
                            .text_center()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child(
                                "No peer has sent them yet. A torrent with few seeders can \
                                 take minutes, or never answer.",
                            ),
                    )
                })
                .child(
                    Button::new("cancel")
                        .label("Cancel")
                        .on_click(cx.listener(|this, _, window, cx| this.back(window, cx))),
                )
                .into_any_element(),
            Page::Episodes { title, table, .. } => v_flex()
                .size_full()
                .child(
                    h_flex()
                        .gap_2()
                        .px_3()
                        .pb_2()
                        .child(
                            Button::new("back")
                                .ghost()
                                .small()
                                .icon(IconName::ArrowLeft)
                                .on_click(cx.listener(|this, _, window, cx| this.back(window, cx))),
                        )
                        .child(div().flex_1().truncate().child(title.clone()))
                        .child(
                            Button::new("play")
                                .primary()
                                .small()
                                .label("Play")
                                .on_click(cx.listener(|this, _, _, cx| this.play_selected(cx))),
                        ),
                )
                .child(div().flex_1().child(framed(DataTable::new(table), cx)))
                .into_any_element(),
        }
    }
}

impl Render for Peerflix {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (background, foreground) = (theme.background, theme.foreground);
        v_flex()
            .key_context("Peerflix")
            .size_full()
            .bg(background)
            .text_color(foreground)
            .on_action(cx.listener(|this, _: &SelectPrev, _, cx| this.select(-1, cx)))
            .on_action(cx.listener(|this, _: &SelectNext, _, cx| this.select(1, cx)))
            .on_action(cx.listener(|this, _: &Confirm, window, cx| this.confirm(window, cx)))
            .on_action(cx.listener(|this, _: &NextCategory, window, cx| {
                this.set_category(this.category.shifted(1), window, cx);
            }))
            .on_action(cx.listener(|this, _: &PrevCategory, window, cx| {
                this.set_category(this.category.shifted(-1), window, cx);
            }))
            .on_action(cx.listener(|this, _: &Back, window, cx| {
                if !matches!(this.page, Page::Results) {
                    this.back(window, cx);
                }
            }))
            .child(self.render_title_bar(window, cx))
            .child(div().flex_1().min_h_0().child(self.render_page(cx)))
            .child(self.streams.clone())
    }
}

impl gpui_kit::Focusable for Peerflix {
    fn focus_handle(&self, cx: &gpui_kit::App) -> FocusHandle {
        self.query.read(cx).focus_handle(cx)
    }
}

/// Frames table as DataTable does when bordered, but square at the top,
/// where it meets what's above it.
fn framed<D: TableDelegate>(table: DataTable<D>, cx: &App) -> impl IntoElement {
    let theme = cx.theme();
    div()
        .size_full()
        .border_1()
        .border_color(theme.border)
        .rounded_b(theme.radius)
        .child(table.bordered(false))
}

/// Moves table's selected row by delta rows, staying within its rows.
fn move_selection<D: TableDelegate>(table: &Entity<TableState<D>>, delta: isize, cx: &mut App) {
    table.update(cx, |table, cx| {
        let rows = table.delegate().rows_count(cx);
        if rows == 0 {
            return;
        }
        let row = match table.selected_row() {
            Some(row) => row.saturating_add_signed(delta).min(rows - 1),
            None => 0,
        };
        table.set_selected_row(row, cx);
    });
}

/// A spinner turning a loader-circle.
fn loader() -> Spinner {
    Spinner::new().icon(Icon::new(AssetIcon::LoaderCircle))
}

/// Tells in a notification that the torrent picked couldn't be opened, and
/// why.
fn couldnt_open(e: &anyhow::Error, window: &mut Window, cx: &mut App) {
    let note = Notification::error(format!("{e:#}")).title("Couldn't open the torrent");
    window.push_notification(note, cx);
}

/// The icon for a search that found nothing, with failed holding the sites,
/// out of sites, whose search failed: no connection when none answered, an
/// alert when they answered with errors, else no results.
fn nothing_icon(failed: &Failed, sites: usize) -> AssetIcon {
    if sites == 0 || failed.count() < sites {
        AssetIcon::SearchX
    } else if failed.errors.is_empty() {
        AssetIcon::WifiOff
    } else {
        AssetIcon::CircleAlert
    }
}

/// Whether query has no search terms, which lists what's new or popular.
fn is_browse(query: &Query) -> bool {
    query.text.trim().is_empty()
}

fn capitalized(s: &str) -> String {
    let mut chars = s.chars();
    chars
        .next()
        .map_or_else(String::new, |c| c.to_uppercase().chain(chars).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pictures_why_nothing_was_found() {
        let failed = |unanswered: &[&'static str], errors: &[&str]| Failed {
            unanswered: unanswered.to_vec(),
            errors: errors.iter().map(ToString::to_string).collect(),
        };
        for (failed, want) in [
            (failed(&[], &[]), AssetIcon::SearchX),
            (failed(&["yts"], &[]), AssetIcon::SearchX),
            (failed(&["yts", "tpb"], &[]), AssetIcon::WifiOff),
            (
                failed(&["yts"], &["searching tpb: 429"]),
                AssetIcon::CircleAlert,
            ),
        ] {
            assert_eq!(nothing_icon(&failed, 2), want, "{failed:?}");
        }
    }

    #[test]
    fn capitalizes() {
        assert_eq!(capitalized("anime"), "Anime");
        assert_eq!(capitalized(""), "");
    }
}
