use std::{
    collections::{HashMap, HashSet, VecDeque},
    ops::Range,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use gpui::{
    AnyElement, App, Bounds, Context, DispatchPhase, Div, Entity, EventEmitter, FocusHandle, Image,
    ImageFormat, KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
    ObjectFit, Pixels, Point, ScrollStrategy, SharedString, Stateful, Task, UniformListDecoration,
    UniformListScrollHandle, WeakEntity, Window, anchored, canvas, deferred, div, point,
    prelude::*, px, relative, rgb, svg, uniform_list,
};

use crate::{
    Tooltip, album_art_placeholder,
    album_search::AlbumSearch,
    button,
    input::TextInput,
    jellyfin::{AlbumQuery, AlbumSort, Artist, Client, Error, Item, PAGE_SIZE},
    library_pages::{LibraryPages, needed_pages},
    playback::AlbumAction,
    theme,
};

const SORT_OPTIONS: [(&str, &str, AlbumSort); 4] = [
    ("sort-title", "Title", AlbumSort::Title),
    ("sort-artist", "Artist", AlbumSort::Artist),
    ("sort-year", "Release year", AlbumSort::Year),
    ("sort-added", "Date added", AlbumSort::Added),
];

pub const NAV_WIDTH: f32 = 56.;

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub enum LibraryKind {
    #[default]
    Albums,
    Artists,
}

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
            Self::Albums => include_bytes!("../assets/icons/filled-lucide/disc-3.svg"),
            Self::Artists => include_bytes!("../assets/icons/filled-lucide/users-round.svg"),
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
    artist: Option<Artist>,
    client: Option<Client>,
    pages: LibraryPages,
    wanted_pages: Vec<usize>,
    loading_pages: HashSet<usize>,
    page_errors: HashMap<usize, String>,
    scroll_drag: Option<f32>,
    search: Entity<TextInput>,
    observed_search: String,
    pending_search: Option<Task<()>>,
    sort_open: bool,
    sort_cursor: usize,
    sort_focus: FocusHandle,
    sort_bounds: Bounds<Pixels>,
    query: AlbumQuery,
    search_results: Option<Arc<AlbumSearch>>,
    search_cancelled: Arc<AtomicBool>,
    session_revision: u64,
    query_revision: u64,
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
        Self {
            kind,
            artist: None,
            client: None,
            pages: LibraryPages::default(),
            wanted_pages: vec![0],
            loading_pages: HashSet::new(),
            page_errors: HashMap::new(),
            scroll_drag: None,
            search,
            observed_search: String::new(),
            pending_search: None,
            sort_open: false,
            sort_cursor: 0,
            sort_focus,
            sort_bounds: Bounds::default(),
            query: AlbumQuery::default(),
            search_results: None,
            search_cancelled: Arc::default(),
            session_revision: 0,
            query_revision: 0,
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
        self.load(cx);
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
        self.load(cx);
    }

    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.client = None;
        self.artist = None;
        self.pending_search = None;
        self.sort_open = false;
        self.session_revision += 1;
        self.query_revision += 1;
        self.reset_pages();
        self.covers.clear();
        self.visible_covers.clear();
        self.pending_covers.clear();
        self.loading_covers.clear();
        self.scroll.scroll_to_item_strict(0, ScrollStrategy::Top);
        cx.notify();
    }

    fn reset_pages(&mut self) {
        self.search_cancelled.store(true, Ordering::Relaxed);
        self.search_cancelled = Arc::default();
        self.search_results = None;
        self.pages = LibraryPages::default();
        self.wanted_pages = vec![0];
        self.loading_pages.clear();
        self.page_errors.clear();
        self.scroll_drag = None;
        self.pending_covers.clear();
        self.covers.clear();
        self.visible_covers.clear();
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else {
            return;
        };
        if self.kind == LibraryKind::Albums
            && self.artist.is_none()
            && !self.query.search.is_empty()
            && self.search_results.is_none()
        {
            if !self.loading_pages.contains(&0) && !self.page_errors.contains_key(&0) {
                self.loading_pages.insert(0);
                let query = self.query.clone();
                let cancelled = self.search_cancelled.clone();
                let revision = self.query_revision;
                let request = cx
                    .background_executor()
                    .spawn(async move { client.search_albums(&query, &cancelled) });
                cx.spawn(async move |this, cx| {
                    let result = request.await;
                    let _ = this.update(cx, |view, cx| {
                        if revision != view.query_revision {
                            return;
                        }
                        view.loading_pages.remove(&0);
                        match result {
                            Ok(search) => {
                                tracing::debug!(total = search.ids.len(), "Album search prepared");
                                view.pages.total = Some(search.ids.len());
                                view.search_results = Some(Arc::new(search));
                                view.load(cx);
                            }
                            Err(Error::Unauthorized) => cx.emit(LibraryEvent::Unauthorized),
                            Err(error) => {
                                tracing::error!(%error, "Album search failed");
                                view.page_errors.insert(0, error.to_string());
                            }
                        }
                        cx.notify();
                    });
                })
                .detach();
            }
            return;
        }
        for page in self.wanted_pages.clone() {
            if self.loading_pages.len() >= 2 {
                break;
            }
            if self.pages.contains(page)
                || self.loading_pages.contains(&page)
                || self.page_errors.contains_key(&page)
                || self
                    .pages
                    .total
                    .is_some_and(|total| page * PAGE_SIZE >= total)
            {
                continue;
            }
            self.loading_pages.insert(page);
            let client = client.clone();
            let query = self.query.clone();
            let kind = self.kind;
            let artist = self.artist.as_ref().and_then(|artist| artist.id.clone());
            let search = self.search_results.clone();
            let revision = self.query_revision;
            let request = cx.background_executor().spawn(async move {
                if kind == LibraryKind::Artists {
                    client.artists(page * PAGE_SIZE, &query.search, query.descending)
                } else if let Some(artist) = artist {
                    client.artist_albums(page * PAGE_SIZE, &artist, &query)
                } else if let Some(search) = search {
                    client.search_page(page * PAGE_SIZE, &search)
                } else {
                    client.albums(page * PAGE_SIZE, &query)
                }
            });
            cx.spawn(async move |this, cx| {
                let result = request.await;
                let _ = this.update(cx, |view, cx| {
                    if revision != view.query_revision {
                        return;
                    }
                    view.loading_pages.remove(&page);
                    match result {
                        Ok(result) => {
                            tracing::debug!(
                                page,
                                loaded = result.items.len(),
                                total = result.total_record_count,
                                "Library page loaded"
                            );
                            if let Err(error) = view.pages.insert(page, result) {
                                view.page_errors.insert(page, error.into());
                            }
                        }
                        Err(Error::Unauthorized) => {
                            cx.emit(LibraryEvent::Unauthorized);
                            return;
                        }
                        Err(error) => {
                            tracing::error!(page, %error, "Library request failed");
                            view.page_errors.insert(page, error.to_string());
                        }
                    }
                    view.load(cx);
                    cx.notify();
                });
            })
            .detach();
        }
    }

    fn viewport(&mut self, rows: Range<usize>, columns: usize, cx: &mut Context<Self>) {
        if !self.active {
            return;
        }
        let total = self.pages.total.unwrap_or(0);
        let albums =
            rows.start.saturating_mul(columns)..rows.end.saturating_mul(columns).min(total);
        let wanted = needed_pages(albums.clone(), total);
        if wanted != self.wanted_pages {
            self.wanted_pages = wanted;
            cx.notify();
        }
        for &page in &self.wanted_pages {
            self.pages.touch(page);
        }
        self.load(cx);
        let ids: Vec<_> = albums
            .filter_map(|index| self.pages.get(index).map(|album| album.id.clone()))
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
            self.query_revision += 1;
            self.reset_pages();
            self.scroll.scroll_to_item_strict(0, ScrollStrategy::Top);
        }
        for page in &self.wanted_pages {
            self.page_errors.remove(page);
        }
        self.load(cx);
        cx.notify();
    }

    fn apply(&mut self, cx: &mut Context<Self>) {
        self.query.search = self.search.read(cx).value().trim().to_owned();
        self.query_revision += 1;
        self.reset_pages();
        self.scroll.scroll_to_item_strict(0, ScrollStrategy::Top);
        self.load(cx);
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

    fn request_cover(&mut self, id: &str) {
        if !self.covers.contains_key(id)
            && !self.pending_covers.iter().any(|pending| pending == id)
            && !self.loading_covers.iter().any(|loading| loading == id)
        {
            self.pending_covers.push_back(id.to_owned());
        }
    }

    fn load_covers(&mut self, cx: &mut Context<Self>) {
        if !self.active {
            return;
        }
        let Some(client) = self.client.clone() else {
            return;
        };
        while self.loading_covers.len() < 4 {
            let Some(id) = self.pending_covers.pop_front() else {
                break;
            };
            self.loading_covers.push(id.clone());
            let client = client.clone();
            let revision = self.session_revision;
            let requested_id = id.clone();
            let request = cx
                .background_executor()
                .spawn(async move { client.cover(&requested_id) });
            cx.spawn(async move |this, cx| {
                let result = request.await;
                let _ = this.update(cx, |view, cx| {
                    if revision != view.session_revision {
                        return;
                    }
                    view.loading_covers.retain(|loading| loading != &id);
                    if matches!(result, Err(Error::Unauthorized)) {
                        cx.emit(LibraryEvent::Unauthorized);
                        return;
                    }
                    if !view.active || !view.visible_covers.contains(&id) {
                        // Client::cover has already persisted successful downloads.
                        view.load_covers(cx);
                        return;
                    }
                    let image = match result {
                        Ok(bytes) => {
                            bytes.map(|bytes| Arc::new(Image::from_bytes(ImageFormat::Png, bytes)))
                        }
                        Err(Error::Unauthorized) => {
                            cx.emit(LibraryEvent::Unauthorized);
                            return;
                        }
                        Err(error) => {
                            tracing::warn!(%error, "Could not load library artwork");
                            None
                        }
                    };
                    view.covers.insert(id, image);
                    view.load_covers(cx);
                    cx.notify();
                });
            })
            .detach();
        }
    }

    fn toggle_sort_menu(&mut self, cx: &mut Context<Self>) {
        self.sort_open = !self.sort_open;
        if self.sort_open {
            self.sort_cursor = SORT_OPTIONS
                .iter()
                .position(|(_, _, sort)| *sort == self.query.sort)
                .unwrap_or(0);
        }
        cx.notify();
    }

    fn choose_sort(&mut self, sort: AlbumSort, cx: &mut Context<Self>) {
        self.sort_open = false;
        self.pending_search = None;
        self.query.sort = sort;
        self.apply(cx);
    }

    fn sort_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        match event.keystroke.key.as_str() {
            "escape" => self.sort_open = false,
            "enter" | "space" => {
                if self.sort_open {
                    self.choose_sort(SORT_OPTIONS[self.sort_cursor].2, cx);
                } else {
                    self.toggle_sort_menu(cx);
                }
            }
            "up" | "down" => {
                if !self.sort_open {
                    self.toggle_sort_menu(cx);
                } else if event.keystroke.key == "down" {
                    self.sort_cursor = (self.sort_cursor + 1) % SORT_OPTIONS.len();
                } else {
                    self.sort_cursor =
                        (self.sort_cursor + SORT_OPTIONS.len() - 1) % SORT_OPTIONS.len();
                }
            }
            _ => return,
        }
        window.prevent_default();
        cx.stop_propagation();
        cx.notify();
    }

    fn sort_menu(&self, cx: &Context<Self>) -> impl IntoElement {
        let trigger_bounds = self.sort_bounds;
        let mut menu = div()
            .id("sort-menu")
            .occlude()
            .w(self.sort_bounds.size.width.max(px(160.)))
            .p_1()
            .rounded_md()
            .bg(rgb(theme::MANTLE))
            .border_1()
            .border_color(rgb(theme::SURFACE1))
            .shadow_lg()
            .flex()
            .flex_col()
            .gap_1()
            .on_mouse_down_out(cx.listener(move |view, event: &MouseDownEvent, _, cx| {
                if !trigger_bounds.contains(&event.position) {
                    view.sort_open = false;
                    cx.notify();
                }
            }));
        for (index, (id, text, sort)) in SORT_OPTIONS.into_iter().enumerate() {
            menu = menu.child(
                choice(id, text, index == self.sort_cursor)
                    .w_full()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .when(self.query.sort == sort, |option| option.child("✓"))
                    .on_click(cx.listener(move |view, _, _, cx| view.choose_sort(sort, cx))),
            );
        }
        deferred(
            anchored()
                .position(self.sort_bounds.bottom_left())
                .offset(point(px(0.), px(4.)))
                .snap_to_window()
                .child(menu),
        )
        .with_priority(1)
    }

    fn filters(&self, cx: &Context<Self>) -> impl IntoElement {
        let selected = SORT_OPTIONS
            .iter()
            .find(|(_, _, sort)| *sort == self.query.sort)
            .map(|(_, label, _)| *label)
            .unwrap_or("Title");
        let entity = cx.entity().downgrade();
        let sort_controls = div()
            .relative()
            .flex()
            .items_center()
            .gap_2()
            .when(self.kind == LibraryKind::Albums, |controls| {
                controls.child(
                    div()
                        .id("sort-select")
                        .track_focus(&self.sort_focus)
                        .relative()
                        .flex_1()
                        .min_w_0()
                        .h(px(42.))
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap_2()
                        .px_3()
                        .rounded_md()
                        .bg(rgb(theme::MANTLE))
                        .border_1()
                        .border_color(rgb(theme::SURFACE1))
                        .cursor_pointer()
                        .focus(|style| style.border_color(rgb(theme::MAUVE)))
                        .hover(|style| style.bg(rgb(theme::SURFACE0)))
                        .child(div().min_w_0().truncate().child(selected))
                        .child("⌄")
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|view, _, window, cx| {
                                window.focus(&view.sort_focus, cx);
                                view.toggle_sort_menu(cx);
                                cx.stop_propagation();
                            }),
                        )
                        .on_key_down(
                            cx.listener(|view, event, window, cx| view.sort_key(event, window, cx)),
                        )
                        .child(
                            canvas(
                                move |bounds, _, cx| {
                                    let _ = entity.update(cx, |view, cx| {
                                        if view.sort_bounds != bounds {
                                            view.sort_bounds = bounds;
                                            if view.sort_open {
                                                cx.notify();
                                            }
                                        }
                                    });
                                },
                                |_, _, _, _| (),
                            )
                            .absolute()
                            .top_0()
                            .left_0()
                            .size_full(),
                        ),
                )
            })
            .when(self.kind == LibraryKind::Artists, |controls| {
                controls.child(div().flex_1().child("Name"))
            })
            .child(
                button(
                    "sort-direction",
                    if self.query.descending { "↓" } else { "↑" },
                    false,
                    theme::SURFACE2,
                )
                .w(px(34.))
                .h(px(42.))
                .p_0()
                .flex_shrink_0()
                .flex()
                .items_center()
                .justify_center()
                .text_xl()
                .on_click(cx.listener(|view, _, _, cx| {
                    view.pending_search = None;
                    view.query.descending = !view.query.descending;
                    view.apply(cx);
                })),
            )
            .when(self.sort_open, |controls| {
                controls.child(self.sort_menu(cx))
            });
        div()
            .id("library-filters")
            .w(px(200.))
            .max_w(relative(0.4))
            .h_full()
            .flex_shrink_0()
            .overflow_y_scroll()
            .bg(rgb(theme::MANTLE))
            .border_r_1()
            .border_color(rgb(theme::SURFACE0))
            .child(
                div()
                    .p_4()
                    .flex()
                    .flex_col()
                    .gap_5()
                    .child(div().text_lg().child("Browse"))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_2()
                            .child(label("Search"))
                            .child(self.search.clone()),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_2()
                            .child(label("Sort by"))
                            .child(sort_controls),
                    )
                    .child(
                        button("reset-filters", "Reset", false, theme::SURFACE2)
                            .on_click(cx.listener(|view, _, _, cx| view.reset(cx))),
                    ),
            )
    }

    fn album_actions(
        &self,
        album: &Item,
        cover: Option<Arc<Image>>,
        width: f32,
        cx: &Context<Self>,
    ) -> Div {
        let play_size = (width * 0.3).clamp(32., 64.);
        let queue_size = (width * 0.2).clamp(24., 40.);
        let mut controls = div()
            .relative()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .gap(px((width * 0.04).clamp(4., 12.)))
            .child(div().size(px(queue_size)).flex_shrink_0());
        for (action, id, label, icon, size) in [
            (
                AlbumAction::Play,
                "cover-play",
                "Play album",
                include_bytes!("../assets/icons/filled-lucide/play.svg").as_slice(),
                play_size,
            ),
            (
                AlbumAction::Queue,
                "cover-queue",
                "Queue album",
                include_bytes!("../assets/icons/filled-lucide/list-plus.svg").as_slice(),
                queue_size,
            ),
        ] {
            let album = album.clone();
            let cover = cover.clone();
            controls = controls.child(
                div()
                    .id(id)
                    .focusable()
                    .aria_label(label)
                    .size(px(size))
                    .flex_shrink_0()
                    .rounded_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(rgb(theme::TEXT))
                    .cursor_pointer()
                    .opacity(0.)
                    .group_hover("album-card", |style| style.opacity(1.))
                    .focus(|style| style.opacity(1.).bg(rgb(theme::LAVENDER)))
                    .hover(|style| style.bg(rgb(theme::LAVENDER)))
                    .tooltip(move |_, cx| cx.new(|_| Tooltip(label)).into())
                    .child(
                        svg()
                            .data(icon)
                            .size(px(size * 0.45))
                            .text_color(rgb(theme::BASE)),
                    )
                    .on_click(cx.listener(move |_, _, _, cx| {
                        cx.emit(LibraryEvent::Action {
                            action,
                            album: Box::new(album.clone()),
                            cover: cover.clone(),
                        });
                        cx.stop_propagation();
                    })),
            );
        }
        div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .child(
                div()
                    .absolute()
                    .top_0()
                    .left_0()
                    .size_full()
                    .bg(rgb(theme::BASE).opacity(0.65))
                    .opacity(0.)
                    .group_hover("album-card", |style| style.opacity(1.)),
            )
            .child(controls)
    }

    fn rows(
        &self,
        range: Range<usize>,
        columns: usize,
        width: f32,
        cx: &Context<Self>,
    ) -> Vec<Div> {
        let mut rows = Vec::new();
        let kind = self.kind;
        for row in range {
            let mut content = div()
                .flex()
                .gap_5()
                .px_6()
                .h(px(width
                    + if kind == LibraryKind::Artists {
                        64.
                    } else {
                        100.
                    }))
                .w_full();
            let start = row * columns;
            let end = (start + columns).min(self.pages.total.unwrap_or(0));
            for index in start..end {
                let Some(item) = self.pages.get(index).cloned() else {
                    content = content.child(library_placeholder(width, kind));
                    continue;
                };
                let cover = self.covers.get(&item.id).cloned().flatten();
                let selected_cover = cover.clone();
                let selected = item.clone();
                let art = div()
                    .relative()
                    .w_full()
                    .aspect_ratio(1.)
                    .rounded_md()
                    .when(kind == LibraryKind::Artists, |art| art.rounded_full())
                    .overflow_hidden()
                    .bg(rgb(theme::SURFACE0))
                    .child(if let Some(cover) = cover {
                        crate::artwork::image(cover)
                            .size_full()
                            .when(kind == LibraryKind::Artists, |avatar| avatar.rounded_full())
                            .object_fit(if kind == LibraryKind::Artists {
                                ObjectFit::Cover
                            } else {
                                ObjectFit::Contain
                            })
                            .with_fallback(move || library_art_placeholder(kind).into_any_element())
                            .into_any_element()
                    } else {
                        library_art_placeholder(kind).into_any_element()
                    })
                    .when(kind == LibraryKind::Albums, |art| {
                        art.child(self.album_actions(&item, selected_cover.clone(), width, cx))
                    });
                content = content.child(
                    div()
                        .id(SharedString::from(item.id.clone()))
                        .group("album-card")
                        .w(px(width))
                        .min_w_0()
                        .focusable()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .rounded_md()
                        .border_1()
                        .border_color(rgb(theme::BASE))
                        .cursor_pointer()
                        .focus(|style| style.border_color(rgb(theme::MAUVE)))
                        .child(art)
                        .when(kind == LibraryKind::Artists, |card| card.text_center())
                        .child(div().mt_2().truncate().child(item.name.clone()))
                        .when(kind == LibraryKind::Albums, |card| {
                            card.child(
                                div()
                                    .truncate()
                                    .text_sm()
                                    .text_color(rgb(theme::SUBTEXT0))
                                    .child(item.artist()),
                            )
                            .child(
                                div().text_sm().text_color(rgb(theme::SUBTEXT0)).child(
                                    item.production_year
                                        .map(|year| year.to_string())
                                        .unwrap_or_default(),
                                ),
                            )
                        })
                        .on_click(cx.listener(move |_, _, _, cx| {
                            cx.emit(LibraryEvent::Open {
                                item: Box::new(selected.clone()),
                                cover: selected_cover.clone(),
                            })
                        })),
                );
            }
            rows.push(content);
        }
        rows
    }

    fn scrollbar(
        &self,
        viewport: Bounds<Pixels>,
        offset: Point<Pixels>,
        content_height: Pixels,
        cx: &Context<Self>,
    ) -> AnyElement {
        let height = f32::from(viewport.size.height);
        let content_height = f32::from(content_height);
        let Some((thumb_top, thumb_height)) =
            scrollbar_thumb(height, content_height, -f32::from(offset.y))
        else {
            return div().into_any_element();
        };
        let entity = cx.entity().downgrade();
        div()
            .size_full()
            .relative()
            .child(
                div()
                    .id("album-scrollbar")
                    .absolute()
                    .right_0()
                    .top(-offset.y)
                    .h(viewport.size.height)
                    .w(px(10.))
                    .rounded_full()
                    .bg(rgb(theme::MANTLE))
                    .cursor_pointer()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |view, event: &MouseDownEvent, _, cx| {
                            let y = f32::from(event.position.y - viewport.origin.y);
                            let grab = if (thumb_top..=thumb_top + thumb_height).contains(&y) {
                                y - thumb_top
                            } else {
                                thumb_height / 2.
                            };
                            view.scroll_drag = Some(grab);
                            view.drag_scrollbar(y, grab, height, content_height, cx);
                            cx.stop_propagation();
                        }),
                    )
                    .child(
                        div()
                            .absolute()
                            .top(px(thumb_top))
                            .left(px(2.))
                            .w(px(6.))
                            .h(px(thumb_height))
                            .rounded_full()
                            .bg(rgb(theme::OVERLAY1))
                            .hover(|thumb| thumb.bg(rgb(theme::LAVENDER))),
                    )
                    .child(
                        canvas(
                            |_, _, _| (),
                            move |_, _, window, _| {
                                let moved = entity.clone();
                                window.on_mouse_event(
                                    move |event: &MouseMoveEvent, phase, _, cx| {
                                        if phase != DispatchPhase::Capture {
                                            return;
                                        }
                                        let _ = moved.update(cx, |view, cx| {
                                            if let Some(grab) = view.scroll_drag {
                                                if event.dragging() {
                                                    view.drag_scrollbar(
                                                        f32::from(
                                                            event.position.y - viewport.origin.y,
                                                        ),
                                                        grab,
                                                        height,
                                                        content_height,
                                                        cx,
                                                    );
                                                } else {
                                                    view.scroll_drag = None;
                                                }
                                            }
                                        });
                                    },
                                );
                                window.on_mouse_event(move |_: &MouseUpEvent, _, _, cx| {
                                    let _ = entity.update(cx, |view, _| view.scroll_drag = None);
                                });
                            },
                        )
                        .size_full(),
                    ),
            )
            .into_any_element()
    }

    fn drag_scrollbar(
        &mut self,
        y: f32,
        grab: f32,
        height: f32,
        content_height: f32,
        cx: &mut Context<Self>,
    ) {
        let offset = scrollbar_offset(y - grab, height, content_height);
        self.scroll
            .0
            .borrow()
            .base_handle
            .set_offset(point(px(0.), px(-offset)));
        cx.notify();
    }
}

struct LibraryGridDecoration {
    page: WeakEntity<LibraryPage>,
    columns: usize,
}

impl UniformListDecoration for LibraryGridDecoration {
    fn compute(
        &self,
        range: Range<usize>,
        bounds: Bounds<Pixels>,
        offset: Point<Pixels>,
        item_height: Pixels,
        count: usize,
        _: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        // Decorations run for the actual viewport, unlike the row callback,
        // which GPUI also calls for measuring offscreen rows.
        self.page
            .update(cx, |view, cx| {
                view.viewport(range, self.columns, cx);
                view.scrollbar(
                    Bounds::new(bounds.origin - offset, bounds.size),
                    offset,
                    item_height * count,
                    cx,
                )
            })
            .unwrap_or_else(|_| div().into_any_element())
    }
}

fn library_art_placeholder(kind: LibraryKind) -> Div {
    if kind == LibraryKind::Albums {
        return album_art_placeholder();
    }
    div()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .child(
            svg()
                .data(kind.icon())
                .size(px(64.))
                .text_color(rgb(theme::OVERLAY1)),
        )
}

fn library_placeholder(width: f32, kind: LibraryKind) -> Div {
    div()
        .w(px(width))
        .flex()
        .flex_col()
        .gap_2()
        .child(
            div()
                .w_full()
                .aspect_ratio(1.)
                .rounded_md()
                .when(kind == LibraryKind::Artists, |art| art.rounded_full())
                .bg(rgb(theme::SURFACE0)),
        )
        .child(
            div()
                .mt_2()
                .w(relative(0.8))
                .h(px(15.))
                .rounded_sm()
                .bg(rgb(theme::SURFACE0)),
        )
        .child(
            div()
                .w(relative(0.6))
                .h(px(12.))
                .rounded_sm()
                .bg(rgb(theme::SURFACE0)),
        )
}

/// Thumb position uses the full content extent, never the number of cached pages.
fn scrollbar_thumb(viewport: f32, content: f32, offset: f32) -> Option<(f32, f32)> {
    if viewport <= 0. || content <= viewport {
        return None;
    }
    let thumb = (viewport * viewport / content).max(24.).min(viewport);
    let top = (offset / (content - viewport)).clamp(0., 1.) * (viewport - thumb);
    Some((top, thumb))
}

fn scrollbar_offset(thumb_top: f32, viewport: f32, content: f32) -> f32 {
    let Some((_, thumb)) = scrollbar_thumb(viewport, content, 0.) else {
        return 0.;
    };
    if viewport <= thumb {
        return 0.;
    }
    (thumb_top / (viewport - thumb)).clamp(0., 1.) * (content - viewport)
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
                            self.pages
                                .total
                                .map(|total| format!("{total} {}", self.kind.noun()))
                                .unwrap_or_else(|| format!("Loading {}…", self.kind.noun())),
                        ),
                    ),
            );
        let total = self.pages.total.unwrap_or(0);
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
        } else if self.pages.total == Some(0) {
            content = content.child(div().px_6().pb_6().text_color(rgb(theme::SUBTEXT0)).child(
                format!("No {} found. Try changing the filters.", self.kind.noun()),
            ));
        }
        if let Some(error) = self
            .wanted_pages
            .iter()
            .find_map(|page| self.page_errors.get(page))
        {
            content = content
                .child(
                    div()
                        .px_6()
                        .py_3()
                        .text_color(rgb(theme::RED))
                        .child(error.clone()),
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

fn label(text: &'static str) -> Div {
    div().text_sm().text_color(rgb(theme::SUBTEXT0)).child(text)
}

fn choice(id: &'static str, text: &'static str, selected: bool) -> Stateful<Div> {
    div()
        .id(id)
        .px_3()
        .py_2()
        .rounded_md()
        .border_1()
        .cursor_pointer()
        .bg(rgb(if selected {
            theme::SURFACE0
        } else {
            theme::MANTLE
        }))
        .border_color(rgb(if selected {
            theme::MAUVE
        } else {
            theme::MANTLE
        }))
        .text_color(rgb(if selected {
            theme::MAUVE
        } else {
            theme::SUBTEXT1
        }))
        .focus(|style| style.border_color(rgb(theme::MAUVE)))
        .hover(|style| style.bg(rgb(theme::SURFACE0)))
        .child(div().min_w_0().truncate().child(text))
}

fn grid_layout(available: f32) -> (usize, f32) {
    let available = available.max(1.);
    let columns = ((available + 20.) / 200.).floor().max(1.) as usize;
    (
        columns,
        (available - (columns - 1) as f32 * 20.) / columns as f32,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrollbar_tracks_the_full_extent_and_dragging_round_trips() {
        assert_eq!(scrollbar_thumb(600., 6000., 0.), Some((0., 60.)));
        assert_eq!(scrollbar_thumb(600., 6000., 5400.), Some((540., 60.)));
        for offset in [0., 1350., 2700., 5400.] {
            let (top, _) = scrollbar_thumb(600., 6000., offset).unwrap();
            assert!((scrollbar_offset(top, 600., 6000.) - offset).abs() < 0.01);
        }
        assert_eq!(scrollbar_offset(-100., 600., 6000.), 0.);
        assert_eq!(scrollbar_offset(10_000., 600., 6000.), 5400.);
    }

    #[test]
    fn scrollbar_handles_small_and_very_large_libraries() {
        assert!(scrollbar_thumb(600., 300., 0.).is_none());
        assert!(scrollbar_thumb(0., 300., 0.).is_none());
        let content = 3_000_000.;
        let (top, thumb) = scrollbar_thumb(600., content, content - 600.).unwrap();
        assert_eq!(thumb, 24.);
        assert_eq!(top, 576.);
        assert_eq!(scrollbar_offset(top, 600., content), content - 600.);
    }

    #[test]
    fn grid_fits_narrow_and_wide_windows() {
        for width in [1., 150., 452., 1052.] {
            let (columns, card) = grid_layout(width);
            assert!(columns >= 1 && card > 0.);
            assert!((columns as f32 * card + (columns - 1) as f32 * 20. - width).abs() < 0.01);
        }
        assert_eq!(grid_layout(452.).0, 2);
        assert_eq!(grid_layout(1052.).0, 5);
    }
}
