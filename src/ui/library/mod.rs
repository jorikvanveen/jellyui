mod artwork;
mod filters;
mod grid;
mod scrollbar;
pub use crate::domain::LibraryKind;
use grid::{LibraryGridDecoration, grid_layout};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    ops::Range,
    sync::Arc,
    time::Duration,
};

use gpui::{
    Bounds, Context, Entity, EventEmitter, FocusHandle, Image, Pixels, ScrollStrategy, Task,
    UniformListScrollHandle, Window, div, prelude::*, rgb, uniform_list,
};

use crate::{
    application::library::{Library, SessionRejected},
    domain::playback::AlbumAction,
    domain::{AlbumQuery, AlbumSort, Artist, Item},
    infrastructure::jellyfin::Client,
    ui::components::button,
    ui::components::input::TextInput,
    ui::theme,
};

const SORT_OPTIONS: [(&str, &str, AlbumSort); 4] = [
    ("sort-title", "Title", AlbumSort::Title),
    ("sort-artist", "Artist", AlbumSort::Artist),
    ("sort-year", "Release year", AlbumSort::Year),
    ("sort-added", "Date added", AlbumSort::Added),
];

pub const NAV_WIDTH: f32 = 56.;

impl LibraryKind {
    pub fn title(self) -> &'static str {
        match self {
            Self::Albums => "Albums",
            Self::Artists => "Artists",
        }
    }

    fn noun(self) -> &'static str {
        match self {
            Self::Albums => "albums",
            Self::Artists => "artists",
        }
    }

    pub fn icon(self) -> &'static [u8] {
        match self {
            Self::Albums => include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/assets/icons/filled-lucide/disc-3.svg"
            )),
            Self::Artists => include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/assets/icons/filled-lucide/users-round.svg"
            )),
        }
    }
}

pub enum LibraryEvent {
    Open {
        item: Box<Item>,
        cover: Option<Arc<Image>>,
    },
    Action {
        action: AlbumAction,
        album: Box<Item>,
        cover: Option<Arc<Image>>,
    },
    BackToArtists,
    Unauthorized,
}

pub struct LibraryPage {
    kind: LibraryKind,
    library: Entity<Library>,
    artist: Option<Artist>,
    client: Option<Client>,
    scroll_drag: Option<f32>,
    search: Entity<TextInput>,
    observed_search: String,
    pending_search: Option<Task<()>>,
    sort_open: bool,
    sort_cursor: usize,
    sort_focus: FocusHandle,
    sort_bounds: Bounds<Pixels>,
    query: AlbumQuery,
    session_revision: u64,
    scroll: UniformListScrollHandle,
    // None means missing/unavailable artwork. A queued cover is absent here.
    active: bool,
    visible_covers: HashSet<String>,
    covers: HashMap<String, Option<Arc<Image>>>,
    pending_covers: VecDeque<String>,
    loading_covers: Vec<String>,
}

impl EventEmitter<LibraryEvent> for LibraryPage {}

impl LibraryPage {
    fn reset_artwork(&mut self) {
        self.scroll_drag = None;
        self.pending_covers.clear();
        self.covers.clear();
        self.visible_covers.clear();
    }

    pub fn new(kind: LibraryKind, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let placeholder = match kind {
            LibraryKind::Albums => "Search albums or artists",
            LibraryKind::Artists => "Search artists",
        };
        let search = cx.new(|cx| TextInput::new(placeholder, false, cx));
        cx.observe(&search, |view, input, cx| {
            let value = input.read(cx).value().to_owned();
            // Cursor/selection notifications must not restart the search timer.
            if value == view.observed_search {
                return;
            }
            view.observed_search = value.clone();
            view.pending_search = Some(cx.spawn(async move |this, cx| {
                cx.background_executor()
                    .timer(Duration::from_millis(200))
                    .await;
                let _ = this.update(cx, |view, cx| {
                    let current = view.search.read(cx).value();
                    if current == value && current.trim() != view.query.search {
                        view.apply(cx);
                    }
                });
            }));
        })
        .detach();
        let sort_focus = cx.focus_handle().tab_stop(true);
        cx.on_blur(&sort_focus, window, |view, _, cx| {
            if view.sort_open {
                view.sort_open = false;
                cx.notify();
            }
        })
        .detach();
        let library = cx.new(|_| Library::new(kind));
        cx.observe(&library, |_, _, cx| cx.notify()).detach();
        cx.subscribe(&library, |_, _, _: &SessionRejected, cx| {
            cx.emit(LibraryEvent::Unauthorized)
        })
        .detach();
        Self {
            kind,
            library,
            artist: None,
            client: None,
            scroll_drag: None,
            search,
            observed_search: String::new(),
            pending_search: None,
            sort_open: false,
            sort_cursor: 0,
            sort_focus,
            sort_bounds: Bounds::default(),
            query: AlbumQuery::default(),
            session_revision: 0,
            scroll: UniformListScrollHandle::new(),
            active: false,
            visible_covers: HashSet::new(),
            covers: HashMap::new(),
            pending_covers: VecDeque::new(),
            loading_covers: Vec::new(),
        }
    }

    pub fn connect(&mut self, client: Client, cx: &mut Context<Self>) {
        self.clear(cx);
        self.client = Some(client);
        self.query.search = self.search.read(cx).value().trim().to_owned();
        self.library.update(cx, |library, cx| {
            library.connect(
                self.client.clone().unwrap(),
                self.artist.clone(),
                self.query.clone(),
                cx,
            )
        });
    }

    pub fn set_active(&mut self, active: bool, cx: &mut Context<Self>) {
        if self.active == active {
            return;
        }
        self.active = active;
        if !active {
            self.covers.clear();
            self.visible_covers.clear();
            self.pending_covers.clear();
        }
        cx.notify();
    }

    pub fn connect_artist(&mut self, client: Client, artist: Artist, cx: &mut Context<Self>) {
        self.clear(cx);
        self.artist = Some(artist);
        self.client = Some(client);
        self.observed_search.clear();
        self.search.update(cx, |input, cx| {
            input.clear(cx);
            input.set_placeholder("Search albums", cx);
        });
        self.query = AlbumQuery::default();
        self.library.update(cx, |library, cx| {
            library.connect(
                self.client.clone().unwrap(),
                self.artist.clone(),
                self.query.clone(),
                cx,
            )
        });
    }

    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.client = None;
        self.artist = None;
        self.pending_search = None;
        self.sort_open = false;
        self.session_revision += 1;
        self.library.update(cx, |library, cx| library.clear(cx));
        self.reset_artwork();
        self.covers.clear();
        self.visible_covers.clear();
        self.pending_covers.clear();
        self.loading_covers.clear();
        self.scroll.scroll_to_item_strict(0, ScrollStrategy::Top);
        cx.notify();
    }

    fn viewport(&mut self, rows: Range<usize>, columns: usize, cx: &mut Context<Self>) {
        if !self.active {
            return;
        }
        let total = self.library.read(cx).pages().total.unwrap_or(0);
        let albums =
            rows.start.saturating_mul(columns)..rows.end.saturating_mul(columns).min(total);
        self.library
            .update(cx, |library, cx| library.viewport(albums.clone(), cx));
        let ids: Vec<_> = albums
            .filter_map(|index| {
                self.library
                    .read(cx)
                    .pages()
                    .get(index)
                    .map(|album| album.id.clone())
            })
            .collect();
        let visible: HashSet<_> = ids.iter().cloned().collect();
        let changed = self.visible_covers != visible;
        self.visible_covers = visible;
        self.covers.retain(|id, _| self.visible_covers.contains(id));
        self.pending_covers
            .retain(|id| self.visible_covers.contains(id));
        if changed {
            cx.notify();
        }
        for id in ids {
            self.request_cover(&id);
        }
        self.load_covers(cx);
    }

    fn retry(&mut self, cx: &mut Context<Self>) {
        if !self.query.search.is_empty() {
            self.reset_artwork();
            self.scroll.scroll_to_item_strict(0, ScrollStrategy::Top);
        }
        self.library.update(cx, |library, cx| library.retry(cx));
        cx.notify();
    }

    fn apply(&mut self, cx: &mut Context<Self>) {
        self.query.search = self.search.read(cx).value().trim().to_owned();
        self.reset_artwork();
        self.scroll.scroll_to_item_strict(0, ScrollStrategy::Top);
        self.library
            .update(cx, |library, cx| library.apply(self.query.clone(), cx));
        cx.notify();
    }

    fn reset(&mut self, cx: &mut Context<Self>) {
        self.pending_search = None;
        self.observed_search.clear();
        self.sort_open = false;
        self.search.update(cx, |input, cx| input.clear(cx));
        self.query = AlbumQuery::default();
        self.apply(cx);
    }
}

impl Render for LibraryPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The queue is 300px wide, capped at 40% on narrow windows. Match the
        // actual sidebars so resizing changes column count without a fixed grid.
        let window_width = f32::from(window.bounds().size.width);
        let remaining = (window_width - NAV_WIDTH).max(1.);
        let body_width = remaining - 300f32.min(window_width * 0.4);
        let available = body_width - 200f32.min(body_width * 0.4) - 48.;
        let (columns, card_width) = grid_layout(available);
        let mut content = div()
            .flex_1()
            .min_w_0()
            .h_full()
            .min_h_0()
            .flex()
            .flex_col()
            .child(
                div()
                    .px_6()
                    .pt_6()
                    .pb_4()
                    .flex()
                    .items_baseline()
                    .justify_between()
                    .gap_3()
                    .child(
                        div()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap_3()
                            .when(self.artist.is_some(), |header| {
                                header.child(
                                    div()
                                        .id("back-to-artists")
                                        .focusable()
                                        .cursor_pointer()
                                        .text_sm()
                                        .text_color(rgb(theme::SUBTEXT0))
                                        .hover(|style| style.text_color(rgb(theme::MAUVE)))
                                        .focus(|style| style.text_color(rgb(theme::MAUVE)))
                                        .child("← Back to artists")
                                        .on_click(cx.listener(|_, _, _, cx| {
                                            cx.emit(LibraryEvent::BackToArtists)
                                        })),
                                )
                            })
                            .child(
                                div().truncate().text_2xl().child(
                                    self.artist
                                        .as_ref()
                                        .map(|artist| artist.name.clone())
                                        .unwrap_or_else(|| self.kind.title().to_owned()),
                                ),
                            ),
                    )
                    .child(
                        div().text_sm().text_color(rgb(theme::SUBTEXT0)).child(
                            self.library
                                .read(cx)
                                .pages()
                                .total
                                .map(|total| format!("{total} {}", self.kind.noun()))
                                .unwrap_or_else(|| format!("Loading {}…", self.kind.noun())),
                        ),
                    ),
            );
        let total = self.library.read(cx).pages().total.unwrap_or(0);
        if total > 0 {
            content = content.child(
                uniform_list(
                    "library-grid",
                    total.div_ceil(columns),
                    cx.processor(move |view, range, _, cx| {
                        view.rows(range, columns, card_width, cx)
                    }),
                )
                .track_scroll(&self.scroll)
                .with_decoration(LibraryGridDecoration {
                    page: cx.entity().downgrade(),
                    columns,
                })
                .flex_1()
                .min_h_0()
                .w_full(),
            );
        } else if self.library.read(cx).pages().total == Some(0) {
            content = content.child(div().px_6().pb_6().text_color(rgb(theme::SUBTEXT0)).child(
                format!("No {} found. Try changing the filters.", self.kind.noun()),
            ));
        }
        if let Some(error) = self.library.read(cx).error() {
            content = content
                .child(
                    div()
                        .px_6()
                        .py_3()
                        .text_color(rgb(theme::RED))
                        .child(error.to_owned()),
                )
                .child(
                    button("retry-library", "Retry", false, theme::SURFACE2)
                        .self_start()
                        .mx_6()
                        .mb_6()
                        .on_click(cx.listener(|view, _, _, cx| view.retry(cx))),
                );
        }
        div()
            .size_full()
            .min_w_0()
            .min_h_0()
            .flex()
            .child(self.filters(cx))
            .child(content)
    }
}
