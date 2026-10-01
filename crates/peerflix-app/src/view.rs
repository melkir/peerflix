//! The window: a search over a category's sites, a torrent's episodes to pick
//! from, and the streams playing.

use std::{sync::Arc, time::Duration};

use gpui_kit::component::{
    ActiveTheme as _, IconName, Sizable as _, Theme,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputEvent, InputState},
    spinner::Spinner,
    tab::{Tab, TabBar},
    table::{DataTable, TableEvent, TableState},
    v_flex,
};
use gpui_kit::{
    AppContext as _, Context, Entity, FocusHandle, Focusable as _, InteractiveElement as _,
    IntoElement, ParentElement as _, Pixels, Render, SharedString, Styled as _, Subscription, Task,
    Window, div, prelude::FluentBuilder as _,
};
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

pub struct Peerflix {
    category: Category,
    endpoints: Endpoints,
    /// The category's sites.
    providers: Vec<Arc<dyn Provider>>,
    query: Entity<InputState>,
    results: Entity<TableState<Results>>,
    /// What went wrong with the last search or torrent, if anything.
    note: SharedString,
    /// The search running, if any; dropping it stops it.
    search: Option<Task<()>>,
    page: Page,
    streams: Entity<Streams>,
    /// The window's width, which the tables' first columns fill.
    width: Pixels,
    _subscriptions: Vec<Subscription>,
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
        let results = cx.new(|cx| TableState::new(Results::new(), window, cx));
        let subscriptions = vec![
            cx.subscribe_in(&query, window, |this, _, event, window, cx| match event {
                InputEvent::Change => this.search(true, cx),
                InputEvent::PressEnter { .. } => this.open_selected(window, cx),
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
            search: None,
            page: Page::Results,
            streams: cx.new(|_| Streams::default()),
            width: window.viewport_size().width,
            _subscriptions: subscriptions,
        };
        this.fit_columns(cx);
        // With no terms, that's the newest anime.
        this.search(false, cx);
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
        self.search(false, cx);
    }

    /// Searches the category's sites for what's typed, after DEBOUNCE if
    /// debounce, replacing the search running. The results so far stay until
    /// the first site answers.
    fn search(&mut self, debounce: bool, cx: &mut Context<Self>) {
        let query = Query::new(&self.query.read(cx).value());
        let providers = self.providers.clone();
        let client = cx.global::<Runtime>().client.clone();
        self.results.update(cx, |table, cx| {
            table.delegate_mut().searching = true;
            cx.notify();
        });
        self.search = Some(cx.spawn(async move |this, cx| {
            if debounce {
                cx.background_executor().timer(DEBOUNCE).await;
            }
            let (found, mut answers) = mpsc::unbounded_channel();
            let failed = cx.update(|cx| {
                let query = query.clone();
                runtime::spawn(cx, async move {
                    search::search(&client, &providers, &query, |site, torrents| {
                        let _ = found.send((site, torrents));
                    })
                    .await
                })
            });
            let mut answered = false;
            while let Some((site, torrents)) = answers.recv().await {
                let first = !std::mem::replace(&mut answered, true);
                let added = this.update(cx, |this, cx| this.add_results(site, torrents, first, cx));
                if added.is_err() {
                    return;
                }
            }
            let failed = failed.await;
            let _ = this.update(cx, |this, cx| {
                this.end_search(&query, &failed, answered, cx)
            });
        }));
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
            let results = table.delegate_mut();
            if first {
                results.found.clear();
            }
            results
                .found
                .extend(torrents.into_iter().map(|t| Found::new(site, t)));
            if first && !results.found.is_empty() {
                table.set_selected_row(0, cx);
            }
            cx.notify();
        });
    }

    /// Ends the search for query, with failed holding the sites whose search
    /// failed, and none having answered with results unless answered.
    fn end_search(
        &mut self,
        query: &Query,
        failed: &Failed,
        answered: bool,
        cx: &mut Context<Self>,
    ) {
        let found = self.results.update(cx, |table, cx| {
            let results = table.delegate_mut();
            results.searching = false;
            if !answered {
                results.found.clear();
            }
            cx.notify();
            results.found.len()
        });
        let sites = self.providers.len();
        self.note = search::summary(self.category, &query.text, found, failed, sites).into();
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

    /// Moves the selected result by delta rows.
    fn select(&mut self, delta: isize, cx: &mut Context<Self>) {
        self.results.update(cx, |table, cx| {
            let rows = table.delegate().found.len();
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

    /// Opens the selected result.
    fn open_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let table = self.results.read(cx);
        let Some(found) = table
            .selected_row()
            .and_then(|row| table.delegate().found.get(row))
        else {
            return;
        };
        let (url, title) = (found.url.clone(), found.title.clone());
        let session = cx.global::<Runtime>().session.clone();
        let fetch = runtime::spawn(cx, async move { play::list(&session, &url).await });
        let listing = cx.spawn_in(window, {
            let title = title.clone();
            async move |this, cx| {
                let listed = fetch.await;
                let _ = this.update_in(cx, |this, window, cx| {
                    this.listed(title, listed, window, cx);
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
        self.note = SharedString::default();
        cx.notify();
    }

    /// Picks the episode to stream from listing, the files of the torrent
    /// titled title, or streams its only video.
    fn listed(
        &mut self,
        title: SharedString,
        listing: anyhow::Result<Listing>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let listing = match listing {
            Ok(listing) => listing,
            Err(e) => {
                self.note = format!("Couldn't open {title}: {e:#}").into();
                return self.back(window, cx);
            }
        };
        if listing.episodes.len() < 2 {
            match pick_file(&listing.files, None) {
                Ok(id) => self.play(title, Arc::new(listing), id, cx),
                Err(e) => self.note = format!("Couldn't open {title}: {e:#}").into(),
            }
            return self.back(window, cx);
        }
        let episodes = Episodes::new(&listing.files, &listing.episodes, self.width);
        let table = cx.new(|cx| {
            let mut table = TableState::new(episodes, window, cx);
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
            listing: Arc::new(listing),
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

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let selected = self.category.index();
        h_flex()
            .gap_3()
            .p_3()
            .child(
                TabBar::new("categories")
                    .segmented()
                    .selected_index(selected)
                    .on_click(cx.listener(|this, ix: &usize, window, cx| {
                        this.set_category(Category::ALL[*ix], window, cx);
                    }))
                    .children(Category::ALL.map(|c| Tab::new().label(capitalized(c.name())))),
            )
            .child(
                div()
                    .flex_1()
                    .child(Input::new(&self.query).cleanable(true)),
            )
    }

    fn render_page(&self, cx: &mut Context<Self>) -> impl IntoElement {
        match &self.page {
            Page::Results => div()
                .size_full()
                .child(DataTable::new(&self.results).stripe(true))
                .into_any_element(),
            Page::Opening { title, slow, .. } => v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .gap_3()
                .child(Spinner::new().large())
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
                .child(div().flex_1().child(DataTable::new(table).stripe(true)))
                .into_any_element(),
        }
    }
}

impl Render for Peerflix {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (background, foreground, muted) =
            (theme.background, theme.foreground, theme.muted_foreground);
        v_flex()
            .key_context("Peerflix")
            .size_full()
            .bg(background)
            .text_color(foreground)
            .on_action(cx.listener(|this, _: &SelectPrev, _, cx| this.select(-1, cx)))
            .on_action(cx.listener(|this, _: &SelectNext, _, cx| this.select(1, cx)))
            .on_action(
                cx.listener(|this, _: &Confirm, window, cx| match this.page {
                    Page::Episodes { .. } => this.play_selected(cx),
                    _ => this.open_selected(window, cx),
                }),
            )
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
            .child(self.render_header(cx))
            .when(!self.note.is_empty(), |el| {
                el.child(
                    div()
                        .px_3()
                        .pb_2()
                        .text_sm()
                        .text_color(muted)
                        .child(self.note.clone()),
                )
            })
            .child(div().flex_1().min_h_0().child(self.render_page(cx)))
            .child(self.streams.clone())
    }
}

impl gpui_kit::Focusable for Peerflix {
    fn focus_handle(&self, cx: &gpui_kit::App) -> FocusHandle {
        self.query.read(cx).focus_handle(cx)
    }
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
    fn capitalizes() {
        assert_eq!(capitalized("anime"), "Anime");
        assert_eq!(capitalized(""), "");
    }
}
